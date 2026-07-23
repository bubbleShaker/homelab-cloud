//! homelab-s3 M3: S3-over-HTTP の最小クライアント（依存ゼロ）。
//!
//! `bin/server.rs` の対になる存在。ベンチや LAN 疎通確認から `ObjectStore` と
//! ほぼ同じ形（put/get/delete）で使えるようにして、計測ロジックを共有できるようにする。
//!
//! 接続モデル: サーバが1接続1リクエスト（`Connection: close`）なので、
//! クライアントも**リクエストごとに `TcpStream::connect`** する。keep-alive はしない。
//! これで往復コストを含んだ正直なレイテンシが測れる（最適化は M5 スコープ）。

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::http::{self, Method};

/// 応答待ちの上限。サーバが黙り込んでもベンチが無限に固まらないようにする。
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// 1レスポンスで受け取る上限バイト数。サーバの body 上限(64MiB)にヘッダ分の余白を足す。
/// 壊れた/悪意あるサーバが無限に流してもクライアントのメモリを食い尽くさないため。
const MAX_RESPONSE: u64 = 64 * 1024 * 1024 + 4 * 1024;

/// HTTP 越しに自作S3を叩くクライアント。`addr` は "host:port"。
pub struct S3Client {
    addr: String,
}

impl S3Client {
    /// 接続先アドレス（"127.0.0.1:8080" など）を覚えるだけ。接続は各操作時に張る。
    pub fn new(addr: impl Into<String>) -> Self {
        Self { addr: addr.into() }
    }

    /// PUT /{bucket}/{key}。2xx 以外はエラーにする。
    pub fn put(&self, bucket: &str, key: &str, value: &[u8]) -> io::Result<()> {
        let path = format!("/{bucket}/{key}");
        let (status, _body) = self.request(Method::Put, &path, value)?;
        expect_success(status)
    }

    /// GET /{bucket}/{key}。200 なら Some(body)、404 なら None、その他はエラー。
    pub fn get(&self, bucket: &str, key: &str) -> io::Result<Option<Vec<u8>>> {
        let path = format!("/{bucket}/{key}");
        let (status, body) = self.request(Method::Get, &path, &[])?;
        match status {
            200 => Ok(Some(body)),
            404 => Ok(None),
            other => Err(unexpected_status(other)),
        }
    }

    /// DELETE /{bucket}/{key}。2xx 以外はエラーにする。
    pub fn delete(&self, bucket: &str, key: &str) -> io::Result<()> {
        let path = format!("/{bucket}/{key}");
        let (status, _body) = self.request(Method::Delete, &path, &[])?;
        expect_success(status)
    }

    /// 1リクエストを送って応答（ステータス + body）を受け取る。
    /// 接続 → リクエスト送信 → レスポンスの頭を読む → Content-Length 分の body を読む。
    fn request(&self, method: Method, path: &str, body: &[u8]) -> io::Result<(u16, Vec<u8>)> {
        let mut stream = TcpStream::connect(&self.addr)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;

        stream.write_all(&http::build_request(method, path, body))?;
        stream.flush()?;

        // サーバは `Connection: close` なので、応答は接続終端まで全部読める。
        // 一括で読んでからヘッダ/body 境界を切り出す。MAX_RESPONSE で上限を掛ける。
        let mut raw = Vec::new();
        Read::by_ref(&mut stream).take(MAX_RESPONSE).read_to_end(&mut raw)?;

        let body_start = find_header_end(&raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
        // body_start はヘッダ末尾の直後（\r\n\r\n の後）。頭はそこまで。
        let head = std::str::from_utf8(&raw[..body_start])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 response head"))?;
        let parsed = http::parse_response_head(head)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        // body は境界の後ろ。宣言された Content-Length に足りなければ（接続断/切り詰め）
        // 短い body を成功として返さずエラーにする——計測を「正直」に保つため。
        let available = raw.len() - body_start;
        if available < parsed.content_length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("body truncated: got {available}, expected {}", parsed.content_length),
            ));
        }
        let body = raw[body_start..body_start + parsed.content_length].to_vec();

        Ok((parsed.status, body))
    }
}

/// `\r\n\r\n` の**直後**の位置（= body 開始オフセット）を返す。無ければ None。
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

/// 2xx を成功とみなし、それ以外はエラーにする。
fn expect_success(status: u16) -> io::Result<()> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(unexpected_status(status))
    }
}

fn unexpected_status(status: u16) -> io::Error {
    io::Error::other(format!("unexpected HTTP status: {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectStore;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::thread;

    /// テスト用に、本物の `server.rs` と同じ扱いをする最小サーバを1リクエストだけ動かす。
    /// ここでは client の往復（build_request → 送信 → 受信 → parse）を検証するのが目的なので、
    /// サーバ本体は使わずに軽量版をインラインで立てる（バイナリ起動の外部依存を避ける）。
    fn spawn_echo_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = thread::spawn(move || {
            let mut store = ObjectStore::open(tempdir()).unwrap();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                // 頭を読む
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let req = http::parse_head(&head).unwrap();
                let mut body = vec![0u8; req.content_length];
                reader.read_exact(&mut body).unwrap();
                let t = http::parse_target(&req.path).unwrap();
                let (status, resp): (u16, Vec<u8>) = match req.method {
                    Method::Put => {
                        store.put(&t.bucket, &t.key, &body).unwrap();
                        (200, Vec::new())
                    }
                    Method::Get => match store.get(&t.bucket, &t.key).unwrap() {
                        Some(v) => (200, v),
                        None => (404, b"not found".to_vec()),
                    },
                    Method::Delete => {
                        store.delete(&t.bucket, &t.key).unwrap();
                        (204, Vec::new())
                    }
                };
                stream.write_all(&http::build_response(status, &resp)).unwrap();
                stream.flush().unwrap();
                // このテストサーバは put→get→delete→get の4リクエストを1接続ずつ処理する。
            }
        });
        (addr, handle)
    }

    fn tempdir() -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "homelab_s3_client_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        d
    }

    #[test]
    fn put_get_delete_roundtrip_over_http() {
        let (addr, _h) = spawn_echo_server();
        let client = S3Client::new(addr);

        client.put("bench", "k1", b"hello-zunda").unwrap();
        assert_eq!(client.get("bench", "k1").unwrap().as_deref(), Some(&b"hello-zunda"[..]));

        client.delete("bench", "k1").unwrap();
        assert_eq!(client.get("bench", "k1").unwrap(), None);
    }
}
