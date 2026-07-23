//! homelab-s3 M3: 自作S3コアを HTTP 越しに叩けるようにする常駐サーバ。
//!
//! `TcpListener` で待受け、1接続を1スレッドで処理する最小形。ObjectStore の
//! put/get/delete は全て `&mut self`（単一の追記ログハンドル）なので、複数接続から
//! 安全に共有するために `Arc<Mutex<ObjectStore>>` で包む。並行性能はまだ狙わない
//! ——スレッドプール/epoll は M5 の削り込みで計測しながら入れる（富豪的に殴らない）。
//!
//! 使い方: `cargo run --bin server -- [DATA_DIR] [BIND_ADDR]`
//!   DATA_DIR  省略時 "./data"、BIND_ADDR 省略時 "127.0.0.1:8080"。
//!
//! ルーティング（S3 最小サブセット）:
//!   PUT    /{bucket}/{key}  body=値 → 200
//!   GET    /{bucket}/{key}          → 値あり 200+body / 無し 404
//!   DELETE /{bucket}/{key}          → 204

use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use homelab_s3::http::{self, Method};
use homelab_s3::ObjectStore;

/// 1リクエストで受け付ける body の上限。無制限にすると巨大 Content-Length で
/// メモリを食い尽くされる（DoS）。M3 の LAN 検証には十分な 64MiB を上限にする。
const MAX_BODY: usize = 64 * 1024 * 1024;

/// リクエストの頭（リクエストライン + ヘッダ）の累積バイト上限。空行が来ない
/// ヘッダを無制限に読むとメモリ枯渇するため（body と同じ DoS 経路）、ここで切る。
const MAX_HEADER: u64 = 16 * 1024;

/// 1接続の読み書きに許す無通信時間。slowloris（Content-Length を大きく宣言して
/// バイトを小出し／無送信）で 1接続1スレッドを枯渇させられるのを防ぐ。
const IO_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "./data".to_string());
    let bind_addr = args.next().unwrap_or_else(|| "127.0.0.1:8080".to_string());

    // ObjectStore を開いて共有ハンドルにする。Arc=複数スレッドで所有共有、
    // Mutex=同時に触るのは1スレッドだけに直列化。
    let store = Arc::new(Mutex::new(ObjectStore::open(&data_dir)?));

    let listener = TcpListener::bind(&bind_addr)?;
    eprintln!("homelab-s3 server: listening on {bind_addr} (data_dir={data_dir})");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        let store = Arc::clone(&store);
        // 1接続 = 1スレッド。ハンドラ内でエラーが出ても接続を落とすだけにして、
        // サーバ全体は生かし続ける（1リクエストの失敗で常駐が死なない）。
        thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &store) {
                eprintln!("connection error: {e}");
            }
        });
    }
    Ok(())
}

/// 1接続を処理する: 頭を読む → body を読む → ルーティング → レスポンス返却。
fn handle_connection(stream: TcpStream, store: &Mutex<ObjectStore>) -> io::Result<()> {
    // 無通信のまま張り付く接続でスレッドを占有させない（slowloris 対策）。
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    // BufReader にして「ヘッダ終端(空行)まで行読み」と「body の残り読み」を扱う。
    let mut reader = BufReader::new(stream);

    // --- 頭を読む: 空行が来るまでの行を連結する。累積を MAX_HEADER で頭打ちに ---
    // Read::take で読み取り総量を上限バイトに縛る。上限に達すると read が 0 を返すので、
    // 終端の空行を見ないまま抜けたら「頭が大きすぎる/途中で切れた」として弾く。
    let mut head = String::new();
    let mut terminated = false;
    {
        let mut limited = (&mut reader).take(MAX_HEADER);
        loop {
            let mut line = String::new();
            let n = limited.read_line(&mut line)?;
            if n == 0 {
                break; // EOF、または MAX_HEADER に到達。
            }
            if line == "\r\n" || line == "\n" {
                terminated = true;
                break; // ヘッダ終端。
            }
            head.push_str(&line);
        }
    }
    if !terminated {
        if head.is_empty() {
            // 相手が何も送らず閉じた（ヘルスチェック等）。静かに終了。
            return Ok(());
        }
        return respond(reader.get_mut(), 400, b"header too large or incomplete");
    }

    // --- 頭をパース。失敗は 4xx/5xx に対応づけて即返す ---
    let request = match http::parse_head(&head) {
        Ok(r) => r,
        // 未対応のメソッド / Transfer-Encoding は「実装していない」なので 501。
        Err(http::ParseError::UnsupportedMethod(_))
        | Err(http::ParseError::UnsupportedTransferEncoding) => {
            return respond(reader.get_mut(), 501, b"not implemented");
        }
        Err(_) => {
            return respond(reader.get_mut(), 400, b"bad request");
        }
    };

    if request.content_length > MAX_BODY {
        return respond(reader.get_mut(), 413, b"payload too large");
    }

    // --- body を Content-Length 分だけ読む ---
    let mut body = vec![0u8; request.content_length];
    reader.read_exact(&mut body)?;

    // --- パスを bucket/key へ分解。壊れていれば 400 ---
    let Some(target) = http::parse_target(&request.path) else {
        return respond(reader.get_mut(), 400, b"path must be /{bucket}/{key}");
    };

    // --- ルーティング。ストア操作はロックを取って直列化する ---
    // poison（ロック保持中に別スレッドがパニック）でも中身を取り出して継続する。
    // 「1リクエストの失敗で常駐全体が死なない」方針のため、連鎖パニックさせない。
    let mut store = store.lock().unwrap_or_else(|e| e.into_inner());
    let (status, resp_body): (u16, Vec<u8>) = match request.method {
        Method::Put => {
            store.put(&target.bucket, &target.key, &body)?;
            (200, Vec::new())
        }
        Method::Get => match store.get(&target.bucket, &target.key)? {
            Some(value) => (200, value),
            None => (404, b"not found".to_vec()),
        },
        Method::Delete => {
            store.delete(&target.bucket, &target.key)?;
            (204, Vec::new())
        }
    };
    // ロックはレスポンス送信前に手放す（送信中に他接続を待たせない）。
    drop(store);

    respond(reader.get_mut(), status, &resp_body)
}

/// レスポンスを1回で書き切る。`build_response` が status 行 + ヘッダ + body を組む。
fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) -> io::Result<()> {
    stream.write_all(&http::build_response(status, body))?;
    stream.flush()
}
