//! homelab-s3 M3: 依存ゼロの最小 HTTP/1.1 パーサ + レスポンス整形。
//!
//! 本プロジェクトの主題「抽象の底が抜けている」を保つため、axum/hyper を使わず
//! 標準ライブラリだけで HTTP を扱う。ここはソケットに依存しない「純粋な変換」だけを
//! 担当し（バイト列 → Request / Response → バイト列）、TCP の read/write は
//! `bin/server.rs` が受け持つ。こうするとパーサ単体をテストできる。
//!
//! スコープ: リクエストライン + Content-Length ヘッダの解釈、パスの bucket/key 分解、
//! ステータス行 + 最小ヘッダの出力。
//! 非スコープ: S3 の XML / 認証(SigV4) / マルチパート / keep-alive / chunked。

use std::fmt;

/// 対応する HTTP メソッド。S3 最小サブセットに必要な3つだけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Put,
    Get,
    Delete,
}

/// リクエストの「頭」= リクエストライン + ヘッダ部分の解釈結果。
/// body 本体はここには含めない（Content-Length 分をサーバが後から読む）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: Method,
    pub path: String,
    /// body のバイト数。ヘッダに無ければ 0（GET/DELETE を想定）。
    pub content_length: usize,
}

/// パスを分解した対象オブジェクト。`/{bucket}/{key}` に対応する。
/// key はスラッシュを含めてよい（`/b/a/b/c` → bucket=b, key=a/b/c）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub bucket: String,
    pub key: String,
}

/// パース失敗の理由。サーバはこれを 4xx/5xx にマッピングする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// リクエストラインが `METHOD SP PATH SP VERSION` の形になっていない。
    MalformedRequestLine,
    /// 対応外メソッド（POST など）。501 に対応。
    UnsupportedMethod(String),
    /// Content-Length の値が数値でない。
    InvalidContentLength,
    /// Content-Length が複数回現れる。値が食い違うと body 境界が曖昧になり
    /// request smuggling の温床になるため、黙って後勝ちにせず弾く。
    ConflictingContentLength,
    /// Transfer-Encoding（chunked 等）は未対応。body 境界を誤ると後続バイトが
    /// 次リクエストのゴミになるため、0 として素通しせず 501 で明示的に拒否する。
    UnsupportedTransferEncoding,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::MalformedRequestLine => write!(f, "malformed request line"),
            ParseError::UnsupportedMethod(m) => write!(f, "unsupported method: {m}"),
            ParseError::InvalidContentLength => write!(f, "invalid content-length"),
            ParseError::ConflictingContentLength => write!(f, "conflicting content-length"),
            ParseError::UnsupportedTransferEncoding => write!(f, "unsupported transfer-encoding"),
        }
    }
}

/// リクエストの頭（`\r\n\r\n` より手前）をパースする。
///
/// 引数の `head` は「リクエストライン + ヘッダ行」を想定。末尾の空行（区切りの
/// `\r\n\r\n`）は含んでいても含んでいなくてもよい（行ループが空行で止まるため）。
pub fn parse_head(head: &str) -> Result<RequestHead, ParseError> {
    // 行区切りは LF で割り、各行末尾の CR を落とす。こうすると CRLF でも LF単体でも
    // 同じように読める（サーバ側の行終端判定と規則を揃える）。
    let mut lines = head.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));

    // 1行目 = リクエストライン。"METHOD SP PATH SP HTTP/1.1"。
    let request_line = lines.next().ok_or(ParseError::MalformedRequestLine)?;
    let mut parts = request_line.split(' ');
    let method_str = parts.next().ok_or(ParseError::MalformedRequestLine)?;
    let path = parts.next().ok_or(ParseError::MalformedRequestLine)?;
    // 3つ目(HTTP/1.1)の存在まで確認する。欠けていれば不正な行として弾く。
    parts.next().ok_or(ParseError::MalformedRequestLine)?;

    let method = match method_str {
        "PUT" => Method::Put,
        "GET" => Method::Get,
        "DELETE" => Method::Delete,
        other => return Err(ParseError::UnsupportedMethod(other.to_string())),
    };

    // 残りの行からヘッダを拾う。今欲しいのは Content-Length だけ。
    // ヘッダ名は大小無視（HTTP 仕様上 case-insensitive）。
    let mut content_length: Option<usize> = None;
    for line in lines {
        if line.is_empty() {
            break; // 空行 = ヘッダ終端。
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| ParseError::InvalidContentLength)?;
            // 重複 Content-Length は後勝ちにせず弾く（smuggling 対策）。
            if content_length.is_some() {
                return Err(ParseError::ConflictingContentLength);
            }
            content_length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // chunked 等は未対応。素通しせず明示的に拒否する。
            return Err(ParseError::UnsupportedTransferEncoding);
        }
    }
    let content_length = content_length.unwrap_or(0);

    Ok(RequestHead {
        method,
        path: path.to_string(),
        content_length,
    })
}

/// パス `/{bucket}/{key}` を bucket と key に分解する。
/// bucket も key も空なら None（ルート `/` や `/bucket` だけは対象外）。
pub fn parse_target(path: &str) -> Option<Target> {
    // 先頭スラッシュを剥がしてから、最初のスラッシュで一度だけ分割する。
    // これで key 側にスラッシュが残せる（S3 のキーは階層を含められる）。
    let trimmed = path.strip_prefix('/')?;
    let (bucket, key) = trimmed.split_once('/')?;
    if bucket.is_empty() || key.is_empty() {
        return None;
    }
    // 二重防御: 現状のコアは key をファイルパスに使わない（追記ログの bucket\0key）が、
    // 将来ファイルベースに変わっても穴にならないよう、境界でパストラバーサルと
    // 制御文字/NUL を弾いておく。bucket は階層を持たない前提でスラッシュも不可。
    if bucket.contains('/') || !is_safe_segment(bucket) || !is_safe_key(key) {
        return None;
    }
    Some(Target {
        bucket: bucket.to_string(),
        key: key.to_string(),
    })
}

/// NUL・制御文字を含まないか。含めばキー/バケットとして拒否する。
fn is_safe_segment(s: &str) -> bool {
    !s.chars().any(|c| c == '\0' || c.is_control())
}

/// key はスラッシュ区切りの階層を許すが、各セグメントに `.`/`..` や制御文字は不可。
/// これで `/b/../etc/passwd` や `/b/a/../../x` のような相対参照を境界で潰す。
fn is_safe_key(key: &str) -> bool {
    is_safe_segment(key) && key.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// HTTP レスポンスを組み立ててバイト列にする。
///
/// ステータスライン + Content-Length + `Connection: close` + 空行 + body。
/// keep-alive はしない（1接続1リクエストで閉じる最小形）ので毎回 close を宣言する。
pub fn build_response(status: u16, body: &[u8]) -> Vec<u8> {
    let reason = reason_phrase(status);
    let mut out = Vec::with_capacity(64 + body.len());
    out.extend_from_slice(
        format!(
            "HTTP/1.1 {status} {reason}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n",
            body.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(body);
    out
}

/// 使うステータスコードの reason phrase。未知コードは "OK" 扱いにしない安全側で "Status"。
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Status",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_put_with_content_length() {
        let head = "PUT /photos/cat.jpg HTTP/1.1\r\nHost: x\r\nContent-Length: 42\r\n\r\n";
        let got = parse_head(head).unwrap();
        assert_eq!(got.method, Method::Put);
        assert_eq!(got.path, "/photos/cat.jpg");
        assert_eq!(got.content_length, 42);
    }

    #[test]
    fn parse_get_without_body_defaults_zero() {
        let head = "GET /photos/cat.jpg HTTP/1.1\r\nHost: x\r\n\r\n";
        let got = parse_head(head).unwrap();
        assert_eq!(got.method, Method::Get);
        assert_eq!(got.content_length, 0);
    }

    #[test]
    fn content_length_header_is_case_insensitive() {
        let head = "PUT /b/k HTTP/1.1\r\ncOnTeNt-LeNgTh:  7 \r\n\r\n";
        assert_eq!(parse_head(head).unwrap().content_length, 7);
    }

    #[test]
    fn delete_is_supported() {
        let head = "DELETE /b/k HTTP/1.1\r\n\r\n";
        assert_eq!(parse_head(head).unwrap().method, Method::Delete);
    }

    #[test]
    fn unsupported_method_is_rejected() {
        let head = "POST /b/k HTTP/1.1\r\n\r\n";
        assert_eq!(
            parse_head(head),
            Err(ParseError::UnsupportedMethod("POST".to_string()))
        );
    }

    #[test]
    fn malformed_request_line_is_rejected() {
        assert_eq!(parse_head("GET_ONLY\r\n\r\n"), Err(ParseError::MalformedRequestLine));
    }

    #[test]
    fn non_numeric_content_length_is_rejected() {
        let head = "PUT /b/k HTTP/1.1\r\nContent-Length: abc\r\n\r\n";
        assert_eq!(parse_head(head), Err(ParseError::InvalidContentLength));
    }

    #[test]
    fn target_splits_bucket_and_key() {
        let t = parse_target("/photos/cat.jpg").unwrap();
        assert_eq!(t.bucket, "photos");
        assert_eq!(t.key, "cat.jpg");
    }

    #[test]
    fn target_keeps_slashes_in_key() {
        let t = parse_target("/photos/2026/07/cat.jpg").unwrap();
        assert_eq!(t.bucket, "photos");
        assert_eq!(t.key, "2026/07/cat.jpg");
    }

    #[test]
    fn target_rejects_root_and_bucket_only() {
        assert_eq!(parse_target("/"), None);
        assert_eq!(parse_target("/photos"), None);
        assert_eq!(parse_target("/photos/"), None);
    }

    #[test]
    fn target_rejects_path_traversal() {
        assert_eq!(parse_target("/b/../etc/passwd"), None);
        assert_eq!(parse_target("/b/a/../../x"), None);
        assert_eq!(parse_target("/b/./x"), None);
    }

    #[test]
    fn target_rejects_nul_and_control_chars() {
        assert_eq!(parse_target("/b/a\0b"), None);
        assert_eq!(parse_target("/b\0/k"), None);
        assert_eq!(parse_target("/b/a\nb"), None);
    }

    #[test]
    fn duplicate_content_length_is_rejected() {
        let head = "PUT /b/k HTTP/1.1\r\nContent-Length: 3\r\nContent-Length: 5\r\n\r\n";
        assert_eq!(parse_head(head), Err(ParseError::ConflictingContentLength));
    }

    #[test]
    fn transfer_encoding_is_rejected() {
        let head = "PUT /b/k HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(parse_head(head), Err(ParseError::UnsupportedTransferEncoding));
    }

    #[test]
    fn lf_only_line_endings_are_accepted() {
        let head = "GET /b/k HTTP/1.1\nContent-Length: 4\n\n";
        let got = parse_head(head).unwrap();
        assert_eq!(got.method, Method::Get);
        assert_eq!(got.content_length, 4);
    }

    #[test]
    fn response_has_status_line_and_body() {
        let bytes = build_response(200, b"hi");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.ends_with("\r\n\r\nhi"));
    }

    #[test]
    fn response_204_has_empty_body() {
        let bytes = build_response(204, b"");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
    }
}
