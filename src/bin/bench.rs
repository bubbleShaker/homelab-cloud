//! M1 ベンチ: `ObjectStore` の PUT/GET スループット（req/s）と p50/p99 を測る。
//!
//! 使い方:
//!   cargo run --release --bin bench -- [--ops N] [--value-size BYTES] [--dir PATH]
//!
//! 注意: `--dir` は既定で OS の一時ディレクトリ（WSL では ext4 のネイティブ FS）。
//! `/mnt/c` 配下を指定すると drvfs/9p のオーバーヘッドを測ってしまい、
//! ストア本体の数字にならないので避けること。
//!
//! release ビジルドで測ること（debug だと最適化が効かず桁が変わる）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use homelab_s3::ObjectStore;
use homelab_s3::metrics::{LatencySummary, throughput};

/// ベンチのパラメータ。CLI 引数から作る。
struct Config {
    ops: usize,
    value_size: usize,
    dir: PathBuf,
}

fn main() {
    let cfg = parse_args();

    // 前回の残骸を消してクリーンな状態から測る（replay 時間や既存 index を混ぜない）。
    let _ = std::fs::remove_dir_all(&cfg.dir);
    let mut store = ObjectStore::open(&cfg.dir).expect("failed to open store");

    // value は固定内容・固定長。内容は測定に影響しないので単純なパターンで埋める。
    let value = vec![0xABu8; cfg.value_size];
    // key は連番。桁を固定しておくと長さのブレが出ない。
    let keys: Vec<String> = (0..cfg.ops).map(|i| format!("key-{i:012}")).collect();

    // ウォームアップ: アロケータ/ページキャッシュ/分岐予測を温める。計測には含めない。
    warmup(&mut store, &value);

    // --- PUT フェーズ ---
    let mut put_lat = Vec::with_capacity(cfg.ops);
    let put_start = Instant::now();
    for k in &keys {
        let t = Instant::now();
        store.put("bench", k, &value).expect("put failed");
        put_lat.push(t.elapsed());
    }
    let put_elapsed = put_start.elapsed();

    // --- GET フェーズ ---
    // 直前に書いた全 key を読む。存在する key なので index ヒット→1 seek のパスを測る。
    let mut get_lat = Vec::with_capacity(cfg.ops);
    let get_start = Instant::now();
    for k in &keys {
        let t = Instant::now();
        let got = store.get("bench", k).expect("get failed");
        // black_box: 戻り値を「使った」ことにして、最適化で GET 自体が消えるのを防ぐ。
        std::hint::black_box(got);
        get_lat.push(t.elapsed());
    }
    let get_elapsed = get_start.elapsed();

    println!(
        "config: ops={} value_size={}B dir={}",
        cfg.ops,
        cfg.value_size,
        cfg.dir.display()
    );
    report("PUT", &mut put_lat, put_elapsed, cfg.value_size);
    report("GET", &mut get_lat, get_elapsed, cfg.value_size);

    // 後始末。ベンチ用データは残さない。
    let _ = std::fs::remove_dir_all(&cfg.dir);
}

/// 計測に入る前に少しだけ回して各種キャッシュを温める。
fn warmup(store: &mut ObjectStore, value: &[u8]) {
    const WARMUP_OPS: usize = 1_000;
    for i in 0..WARMUP_OPS {
        let k = format!("warmup-{i}");
        store.put("warmup", &k, value).expect("warmup put failed");
        std::hint::black_box(store.get("warmup", &k).expect("warmup get failed"));
    }
}

/// 1フェーズ分の結果を表で出す。`lat` は要約時にソートされるので `&mut`。
fn report(name: &str, lat: &mut [Duration], elapsed: Duration, value_size: usize) {
    let s = LatencySummary::from_samples(lat).expect("no samples");
    let tput = throughput(s.count, elapsed);
    let mib_per_sec = tput * value_size as f64 / (1024.0 * 1024.0);

    println!("=== {name} (ops={}, value={}B) ===", s.count, value_size);
    println!("  throughput : {tput:.0} req/s   ({mib_per_sec:.1} MiB/s)");
    println!(
        "  latency    : min {}  mean {}  p50 {}  p90 {}  p99 {}  p99.9 {}  max {}",
        fmt_dur(s.min),
        fmt_dur(s.mean),
        fmt_dur(s.p50),
        fmt_dur(s.p90),
        fmt_dur(s.p99),
        fmt_dur(s.p999),
        fmt_dur(s.max),
    );
}

/// Duration を桁に応じて ns/µs/ms/s の読みやすい単位で表す。
fn fmt_dur(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.1}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", ns as f64 / 1_000_000_000.0)
    }
}

/// CLI 引数をパースする。未知の引数は使い方を出して終了する。
fn parse_args() -> Config {
    let mut ops: usize = 100_000;
    let mut value_size: usize = 256;
    let mut dir = std::env::temp_dir().join("homelab_s3_bench");

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--ops" => ops = parse_next(&mut args, "--ops"),
            "--value-size" => value_size = parse_next(&mut args, "--value-size"),
            "--dir" => {
                dir = PathBuf::from(next_arg(&mut args, "--dir"));
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}\n");
                print_help();
                std::process::exit(2);
            }
        }
    }

    if ops == 0 {
        eprintln!("--ops must be >= 1");
        std::process::exit(2);
    }

    Config {
        ops,
        value_size,
        dir,
    }
}

/// `--flag VALUE` の VALUE を取り出す。無ければエラー終了。
fn next_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    match args.next() {
        Some(v) => v,
        None => {
            eprintln!("missing value for {flag}");
            std::process::exit(2);
        }
    }
}

/// `--flag VALUE` の VALUE を数値としてパースする。
fn parse_next(args: &mut impl Iterator<Item = String>, flag: &str) -> usize {
    let raw = next_arg(args, flag);
    match raw.parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("invalid number for {flag}: {raw}");
            std::process::exit(2);
        }
    }
}

fn print_help() {
    eprintln!(
        "homelab-s3 bench — PUT/GET のスループットと p50/p99 を測る\n\n\
         USAGE:\n  \
         cargo run --release --bin bench -- [OPTIONS]\n\n\
         OPTIONS:\n  \
         --ops N            計測する PUT/GET の回数 (default: 100000)\n  \
         --value-size BYTES 1オブジェクトの value サイズ (default: 256)\n  \
         --dir PATH         データディレクトリ (default: <tmp>/homelab_s3_bench)\n  \
         -h, --help         この使い方を表示\n\n\
         NOTE: --dir に /mnt/c 配下を指定すると drvfs のオーバーヘッドを測るので避ける。"
    );
}
