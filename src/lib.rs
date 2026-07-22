//! homelab-s3 M0: ローカル自作オブジェクトストレージの心臓部。
//!
//! 設計は Bitcask（Riak の内部ストレージ）と同じ「追記ログ + インメモリ index」。
//! - 書き込みは全部ログ末尾への append（シーケンシャルなので速い）
//! - どの key の最新値がログのどこにあるかを HashMap で覚えておく
//!
//! M0 のスコープ: PUT / GET / DELETE(tombstone) と、再起動時のログ replay による index 復元。
//! 非スコープ（後続マイルストーン）: HTTP/S3互換API、fsync 耐久性、log compaction、
//! zero-copy 最適化、ネットワーク/分散。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// 計測ロジック（レイテンシ分布・スループット）。M1 のベンチが使う。
pub mod metrics;

/// ログ内の1レコードのヘッダ長。
/// flags(1) + key_len(4) + value_len(4) = 9 バイト固定。
/// 固定長にしておくと、offset 計算が足し算だけで済む。
const HEADER_LEN: u64 = 1 + 4 + 4;

const FLAG_NORMAL: u8 = 0;
/// 削除マーカー。value を持たず、replay 時にその key を index から消す。
const FLAG_TOMBSTONE: u8 = 1;

/// index が指す「value 本体がログのどこにあるか」。
/// offset にジャンプして size バイト読めば値が取れる。
#[derive(Debug, Clone, Copy)]
struct ValueLoc {
    offset: u64,
    size: u32,
}

/// 追記ログ + index からなる最小オブジェクトストア。
pub struct ObjectStore {
    /// 追記先ログ。read も write もする1本のハンドル。
    log: File,
    /// ログ末尾の位置。append 先であり、次レコードの開始 offset。
    write_offset: u64,
    /// "bucket/key" -> 最新値の位置。
    index: HashMap<String, ValueLoc>,
}

impl ObjectStore {
    /// data_dir にログファイルを開いて（無ければ作って）ストアを起動する。
    ///
    /// NOTE: O_APPEND は使わず、read+write で開いて offset を自前で管理する。
    /// こうすると GET の seek(読み) と PUT の書き込みを1ハンドルで扱える。
    pub fn open(data_dir: impl AsRef<Path>) -> io::Result<Self> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir)?;
        let log_path: PathBuf = data_dir.join("00000.log");

        let mut log = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false) // 既存ログは消さない。中身を replay して index を復元する。
            .open(&log_path)?;

        // 既存ログを頭から舐めて index を再構築する（Bitcask の肝）。
        // レコードは書かれた順に並ぶので、同じ key は後のレコードが勝つ。
        // tombstone に出会ったらその key は index から消す。
        let (index, write_offset) = replay_log(&mut log)?;

        Ok(Self {
            log,
            write_offset,
            index,
        })
    }

    /// bucket/key に value を保存する。
    /// ログ末尾に1レコード append し、value 本体の位置を index に記録する。
    pub fn put(&mut self, bucket: &str, key: &str, value: &[u8]) -> io::Result<()> {
        let composite = compose_key(bucket, key);
        let key_bytes = composite.as_bytes();

        // レコード = [flags][key_len][value_len][key][value]。
        // 数値は little-endian 固定で書く（プラットフォーム差を消すため）。
        let mut record =
            Vec::with_capacity(HEADER_LEN as usize + key_bytes.len() + value.len());
        record.push(FLAG_NORMAL);
        record.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
        record.extend_from_slice(&(value.len() as u32).to_le_bytes());
        record.extend_from_slice(key_bytes);
        record.extend_from_slice(value);

        // ログ末尾へ書き込む。seek で位置を明示してから write。
        self.log.seek(SeekFrom::Start(self.write_offset))?;
        self.log.write_all(&record)?;

        // value 本体の開始位置 = レコード開始 + ヘッダ + key長。
        let value_offset = self.write_offset + HEADER_LEN + key_bytes.len() as u64;
        self.index.insert(
            composite,
            ValueLoc {
                offset: value_offset,
                size: value.len() as u32,
            },
        );

        // 次レコードの開始位置を進める。
        self.write_offset += record.len() as u64;
        Ok(())
    }

    /// bucket/key の最新値を取り出す。無ければ Ok(None)。
    /// index で位置を引き、ログのその offset に seek して size バイト読む。
    pub fn get(&mut self, bucket: &str, key: &str) -> io::Result<Option<Vec<u8>>> {
        let composite = compose_key(bucket, key);
        let Some(loc) = self.index.get(&composite).copied() else {
            return Ok(None);
        };

        let mut buf = vec![0u8; loc.size as usize];
        self.log.seek(SeekFrom::Start(loc.offset))?;
        self.log.read_exact(&mut buf)?;
        Ok(Some(buf))
    }

    /// bucket/key を削除する。value を持たない tombstone レコードを append し、
    /// index から消す。ログ上の古いデータは残る（回収は M5 の compaction）。
    /// key が無くても tombstone は書く（冪等・再起動後も削除が残るように）。
    pub fn delete(&mut self, bucket: &str, key: &str) -> io::Result<()> {
        let composite = compose_key(bucket, key);
        let key_bytes = composite.as_bytes();

        // tombstone = [FLAG_TOMBSTONE][key_len][value_len=0][key]。
        let mut record = Vec::with_capacity(HEADER_LEN as usize + key_bytes.len());
        record.push(FLAG_TOMBSTONE);
        record.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(key_bytes);

        self.log.seek(SeekFrom::Start(self.write_offset))?;
        self.log.write_all(&record)?;
        self.write_offset += record.len() as u64;

        self.index.remove(&composite);
        Ok(())
    }
}

/// ログを頭から順に読み、index と末尾 offset を復元する。
///
/// 各レコード = [flags][key_len][value_len][key][value]。
/// 固定長ヘッダを読んで key_len/value_len を得れば、そのまま次レコードへ進める。
fn replay_log(log: &mut File) -> io::Result<(HashMap<String, ValueLoc>, u64)> {
    let mut index = HashMap::new();
    let end = log.metadata()?.len();
    log.seek(SeekFrom::Start(0))?;

    let mut offset: u64 = 0;
    let mut header = [0u8; HEADER_LEN as usize];
    loop {
        // 残りがヘッダ長に満たなければ、途中で切れた末尾とみなして打ち切る。
        // （クラッシュ後の部分書き込みはここで安全に切り捨てられる）
        if offset + HEADER_LEN > end {
            break;
        }
        log.read_exact(&mut header)?;
        let flags = header[0];
        let key_len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as u64;
        let value_len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as u64;

        // レコード全体がファイル内に収まるか検証する。収まらなければ破損末尾として打ち切る。
        // これで「巨大 len ヘッダによる OOM」と「範囲外レコードの index 登録」を同時に防ぐ。
        let record_end = offset + HEADER_LEN + key_len + value_len;
        if record_end > end {
            break;
        }

        // key を読む。PUT 時のキーは常に有効 UTF-8。不正なら破損とみなして打ち切る。
        let mut key_buf = vec![0u8; key_len as usize];
        log.read_exact(&mut key_buf)?;
        let Ok(composite) = String::from_utf8(key_buf) else {
            break;
        };

        let value_offset = offset + HEADER_LEN + key_len;
        if flags == FLAG_TOMBSTONE {
            index.remove(&composite);
        } else {
            index.insert(
                composite,
                ValueLoc {
                    offset: value_offset,
                    size: value_len as u32,
                },
            );
        }

        // value 本体は読み飛ばして次レコードへ。
        log.seek(SeekFrom::Current(value_len as i64))?;
        offset = record_end;
    }

    // offset は「健全なレコードの終端」= 次の書き込み開始位置。
    // 破損末尾があった場合、そのゴミは次の PUT で上書きされる。
    Ok((index, offset))
}

/// index のキーを組み立てる。
/// 区切りにヌル文字を使うのは、bucket/key に `/` が含まれても
/// ("a","b/c") と ("a/b","c") のように衝突しないようにするため。
fn compose_key(bucket: &str, key: &str) -> String {
    format!("{bucket}\0{key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト毎に固有の一時ディレクトリを用意し、後始末する RAII ヘルパ。
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            // pid + カウンタで衝突を避ける（並列テストでも別ディレクトリになる）。
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("hl_s3_{}_{tag}_{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn put_then_get() {
        let dir = TempDir::new("put_get");
        let mut s = ObjectStore::open(dir.path()).unwrap();
        s.put("b", "k", b"hello").unwrap();
        assert_eq!(s.get("b", "k").unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(s.get("b", "missing").unwrap(), None);
    }

    #[test]
    fn overwrite_returns_latest() {
        let dir = TempDir::new("overwrite");
        let mut s = ObjectStore::open(dir.path()).unwrap();
        s.put("b", "k", b"old").unwrap();
        s.put("b", "k", b"newer!!").unwrap();
        assert_eq!(s.get("b", "k").unwrap().as_deref(), Some(&b"newer!!"[..]));
    }

    #[test]
    fn get_after_delete_is_none() {
        let dir = TempDir::new("delete");
        let mut s = ObjectStore::open(dir.path()).unwrap();
        s.put("b", "k", b"v").unwrap();
        s.delete("b", "k").unwrap();
        assert_eq!(s.get("b", "k").unwrap(), None);
    }

    #[test]
    fn survives_reopen() {
        let dir = TempDir::new("reopen");
        {
            let mut s = ObjectStore::open(dir.path()).unwrap();
            s.put("b", "keep", b"stay").unwrap();
            s.put("b", "gone", b"x").unwrap();
            s.put("b", "keep", b"updated").unwrap(); // 上書きも復元されるか
            s.delete("b", "gone").unwrap();
        } // ストアを閉じる（ドロップ）

        // 開き直すとログから index が再構築される。
        let mut s = ObjectStore::open(dir.path()).unwrap();
        assert_eq!(s.get("b", "keep").unwrap().as_deref(), Some(&b"updated"[..]));
        assert_eq!(s.get("b", "gone").unwrap(), None);

        // 再起動後も書き込みが末尾から続けられること。
        s.put("b", "fresh", b"z").unwrap();
        assert_eq!(s.get("b", "fresh").unwrap().as_deref(), Some(&b"z"[..]));
    }

    #[test]
    fn corrupt_tail_is_ignored() {
        let dir = TempDir::new("corrupt");
        {
            let mut s = ObjectStore::open(dir.path()).unwrap();
            s.put("b", "good", b"value").unwrap();
        }

        // クラッシュ後の壊れた末尾を模す: key_len=1 だが value_len=0xffffffff(~4GiB)、
        // 本体が続かない不正ヘッダを注入する。検証が無ければ巨大確保で OOM する形。
        let log_path = dir.path().join("00000.log");
        let mut f = OpenOptions::new().append(true).open(&log_path).unwrap();
        f.write_all(&[FLAG_NORMAL, 1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]).unwrap();
        drop(f);

        // panic/OOM せずに開け、正常な key はそのまま読めること。
        let mut s = ObjectStore::open(dir.path()).unwrap();
        assert_eq!(s.get("b", "good").unwrap().as_deref(), Some(&b"value"[..]));

        // 破損末尾は上書きされ、書き込みを継続できること。
        s.put("b", "after", b"z").unwrap();
        assert_eq!(s.get("b", "after").unwrap().as_deref(), Some(&b"z"[..]));
    }
}
