//! 計測ロジック: レイテンシ分布とスループットを求める純粋関数群。
//!
//! I/O を一切持たないので、`cargo test` で計算の正しさを固定できる。
//! ベンチ本体（`src/bin/bench.rs`）は「op ごとの所要時間を集める」ことに専念し、
//! 「その列から p50/p99 を出す」責務はここに分離する（テスト可能性と単一責任のため）。

use std::time::Duration;

/// レイテンシ列の要約統計。ベンチが1フェーズ（PUT/GET）ごとに1つ作る。
#[derive(Debug, Clone, Copy)]
pub struct LatencySummary {
    pub count: usize,
    pub min: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub p50: Duration,
    pub p90: Duration,
    pub p99: Duration,
    pub p999: Duration,
}

impl LatencySummary {
    /// レイテンシ列から要約を作る。空なら `None`。
    ///
    /// 破壊的にソートするので `&mut` を取る。分位点は「ソート済み列の N 番目」で求まるため、
    /// 余計なコピーを作らずに済ませる（データを1回だけ並べ替える低コストな方針）。
    pub fn from_samples(samples: &mut [Duration]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        let count = samples.len();

        // 平均は u128 ナノ秒で総和を取ってから割る。
        // Duration / u32 だと件数が u32 を超えた時に破綻するので、ナノ秒経由にする。
        let sum_ns: u128 = samples.iter().map(|d| d.as_nanos()).sum();
        let mean = Duration::from_nanos((sum_ns / count as u128) as u64);

        Some(Self {
            count,
            min: samples[0],
            max: samples[count - 1],
            mean,
            p50: percentile(samples, 50.0),
            p90: percentile(samples, 90.0),
            p99: percentile(samples, 99.0),
            p999: percentile(samples, 99.9),
        })
    }
}

/// nearest-rank 法で分位点を返す。`sorted` はソート済み・非空である前提。
///
/// rank = ceil(p/100 * N) を 1..=N にクランプし、0-indexed に直して引く。
/// 例: N=10 で p50 なら rank=5 → 5番目（0-indexed の 4）。補間はしない
/// （素朴で説明しやすい定義を選ぶ。M5 で分布を見る時に挙動がブレないのが利点）。
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted[idx]
}

/// スループット（ops/sec）。経過時間が 0 以下なら 0 を返す（0除算回避）。
pub fn throughput(count: usize, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        0.0
    } else {
        count as f64 / secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn percentile_nearest_rank_on_known_data() {
        // 10..=100 の10要素。nearest-rank の定義を数値で固定する。
        let mut s: Vec<Duration> = (1..=10).map(|i| ms(i * 10)).collect();
        let sum = LatencySummary::from_samples(&mut s).unwrap();
        assert_eq!(sum.count, 10);
        assert_eq!(sum.min, ms(10));
        assert_eq!(sum.max, ms(100));
        assert_eq!(sum.mean, ms(55)); // (10+..+100)/10 = 55
        assert_eq!(sum.p50, ms(50)); // ceil(0.5*10)=5 -> 5番目
        assert_eq!(sum.p90, ms(90)); // ceil(0.9*10)=9 -> 9番目
        assert_eq!(sum.p99, ms(100)); // ceil(0.99*10)=10 -> 10番目
        assert_eq!(sum.p999, ms(100)); // ceil(0.999*10)=10 -> 10番目
    }

    #[test]
    fn sorts_unsorted_input() {
        // 入力が逆順でも分位点が正しく出ること（内部でソートしている証明）。
        let mut s = vec![ms(100), ms(10), ms(50), ms(30), ms(80)];
        let sum = LatencySummary::from_samples(&mut s).unwrap();
        assert_eq!(sum.min, ms(10));
        assert_eq!(sum.max, ms(100));
        assert_eq!(sum.p50, ms(50)); // ceil(0.5*5)=3 -> 3番目(=50)
    }

    #[test]
    fn single_sample_all_percentiles_equal() {
        let mut s = vec![ms(42)];
        let sum = LatencySummary::from_samples(&mut s).unwrap();
        assert_eq!(sum.min, ms(42));
        assert_eq!(sum.max, ms(42));
        assert_eq!(sum.p50, ms(42));
        assert_eq!(sum.p99, ms(42));
        assert_eq!(sum.p999, ms(42));
    }

    #[test]
    fn empty_is_none() {
        let mut s: Vec<Duration> = vec![];
        assert!(LatencySummary::from_samples(&mut s).is_none());
    }

    #[test]
    fn throughput_basic_and_zero() {
        assert_eq!(throughput(1000, Duration::from_secs(2)), 500.0);
        assert_eq!(throughput(1000, Duration::ZERO), 0.0);
    }
}
