//! Exact percentiles over recorded samples (microseconds in, milliseconds out).
use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Percentiles {
    pub count: usize,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

fn ms(us: u64) -> f64 {
    (us as f64 / 1000.0 * 1000.0).round() / 1000.0
}

/// Nearest-rank percentiles; an empty input gives all zeros.
pub fn percentiles(mut samples: Vec<u64>) -> Percentiles {
    if samples.is_empty() {
        return Percentiles::default();
    }
    samples.sort_unstable();
    let n = samples.len();
    let at = |p: f64| {
        let rank = ((p / 100.0) * n as f64).ceil() as usize;
        samples[rank.clamp(1, n) - 1]
    };
    let sum: u128 = samples.iter().map(|v| u128::from(*v)).sum();
    Percentiles {
        count: n,
        mean_ms: ((sum as f64 / n as f64) / 1000.0 * 1000.0).round() / 1000.0,
        p50_ms: ms(at(50.0)),
        p95_ms: ms(at(95.0)),
        p99_ms: ms(at(99.0)),
        max_ms: ms(samples[n - 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank() {
        let p = percentiles((1..=100).map(|v| v * 1000).collect());
        assert_eq!(p.count, 100);
        assert_eq!(p.p50_ms, 50.0);
        assert_eq!(p.p95_ms, 95.0);
        assert_eq!(p.p99_ms, 99.0);
        assert_eq!(p.max_ms, 100.0);
        assert_eq!(p.mean_ms, 50.5);
        assert_eq!(percentiles(vec![]), Percentiles::default());
        assert_eq!(percentiles(vec![1500]).p99_ms, 1.5);
    }
}
