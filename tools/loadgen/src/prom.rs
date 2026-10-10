//! Minimal OpenMetrics/Prometheus text parsing for the gateway's own
//! histograms and counters: scrape every replica before and after a run,
//! subtract, sum across replicas and estimate quantiles like PromQL's
//! `histogram_quantile` (linear interpolation inside a bucket).
use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

fn parse_labels(text: &str) -> Option<Vec<(String, String)>> {
    let mut labels = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let (name, after) = rest.split_once('=')?;
        let after = after.strip_prefix('"')?;
        let mut value = String::new();
        let mut chars = after.char_indices();
        let end = loop {
            let (i, c) = chars.next()?;
            match c {
                '\\' => match chars.next()?.1 {
                    'n' => value.push('\n'),
                    other => value.push(other),
                },
                '"' => break i,
                c => value.push(c),
            }
        };
        labels.push((name.trim().to_owned(), value));
        rest = after[end + 1..].trim_start_matches(',').trim_start();
    }
    Some(labels)
}

/// Samples of a text exposition; comments and malformed lines are skipped.
pub fn parse(text: &str) -> Vec<Sample> {
    let mut samples = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, labels, rest) = match line.find('{') {
            Some(open) => {
                let Some(close) = line.rfind('}') else {
                    continue;
                };
                let Some(labels) = parse_labels(&line[open + 1..close]) else {
                    continue;
                };
                (&line[..open], labels, &line[close + 1..])
            }
            None => match line.split_once(' ') {
                Some((name, rest)) => (name, Vec::new(), rest),
                None => continue,
            },
        };
        let Some(value) = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
        else {
            continue;
        };
        samples.push(Sample {
            name: name.to_owned(),
            labels,
            value,
        });
    }
    samples
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Histogram {
    /// Cumulative counts by upper bound, ascending (`+Inf` last).
    pub buckets: Vec<(f64, f64)>,
    pub sum: f64,
    pub count: f64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Summary {
    pub count: u64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

impl Histogram {
    /// `self - before`, bucket by bucket (counter resets clamp at zero).
    pub fn delta(&self, before: Option<&Histogram>) -> Histogram {
        let Some(before) = before else {
            return self.clone();
        };
        let lookup = |le: f64| {
            before
                .buckets
                .iter()
                .find(|(b, _)| *b == le)
                .map_or(0.0, |(_, n)| *n)
        };
        Histogram {
            buckets: self
                .buckets
                .iter()
                .map(|(le, n)| (*le, (n - lookup(*le)).max(0.0)))
                .collect(),
            sum: (self.sum - before.sum).max(0.0),
            count: (self.count - before.count).max(0.0),
        }
    }

    pub fn merge(&mut self, other: &Histogram) {
        if self.buckets.is_empty() {
            *self = other.clone();
            return;
        }
        for (le, n) in &other.buckets {
            match self.buckets.iter_mut().find(|(b, _)| b == le) {
                Some(bucket) => bucket.1 += n,
                None => self.buckets.push((*le, *n)),
            }
        }
        self.buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.sum += other.sum;
        self.count += other.count;
    }

    /// PromQL-style estimate in seconds; `None` without observations. A
    /// quantile in the `+Inf` bucket returns the largest finite bound.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let total = self.buckets.last()?.1;
        if total <= 0.0 {
            return None;
        }
        let rank = q * total;
        let mut previous = (0.0, 0.0);
        for &(le, count) in &self.buckets {
            if count >= rank {
                if le.is_infinite() {
                    return Some(previous.0);
                }
                let in_bucket = count - previous.1;
                if in_bucket <= 0.0 {
                    return Some(le);
                }
                return Some(previous.0 + (le - previous.0) * (rank - previous.1) / in_bucket);
            }
            previous = (le, count);
        }
        Some(previous.0)
    }

    pub fn summary(&self) -> Summary {
        let ms = |q| self.quantile(q).map_or(0.0, |s| round3(s * 1000.0));
        Summary {
            count: self.count as u64,
            mean_ms: if self.count > 0.0 {
                round3(self.sum / self.count * 1000.0)
            } else {
                0.0
            },
            p50_ms: ms(0.5),
            p95_ms: ms(0.95),
            p99_ms: ms(0.99),
        }
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Series key: labels other than `le`, as `k=v,k=v` in exposition order.
fn key(labels: &[(String, String)]) -> String {
    labels
        .iter()
        .filter(|(k, _)| k != "le")
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Every series of histogram `metric` (base name without `_bucket`).
pub fn histograms(samples: &[Sample], metric: &str) -> BTreeMap<String, Histogram> {
    let (bucket, sum, count) = (
        format!("{metric}_bucket"),
        format!("{metric}_sum"),
        format!("{metric}_count"),
    );
    let mut out: BTreeMap<String, Histogram> = BTreeMap::new();
    for s in samples {
        if s.name == bucket {
            let Some(le) =
                s.labels
                    .iter()
                    .find(|(k, _)| k == "le")
                    .and_then(|(_, v)| match v.as_str() {
                        "+Inf" => Some(f64::INFINITY),
                        v => v.parse().ok(),
                    })
            else {
                continue;
            };
            out.entry(key(&s.labels))
                .or_default()
                .buckets
                .push((le, s.value));
        } else if s.name == sum {
            out.entry(key(&s.labels)).or_default().sum = s.value;
        } else if s.name == count {
            out.entry(key(&s.labels)).or_default().count = s.value;
        }
    }
    for h in out.values_mut() {
        h.buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    out
}

/// Every series of counter `name` (with or without the `_total` suffix).
pub fn counters(samples: &[Sample], name: &str) -> BTreeMap<String, f64> {
    let total = format!("{name}_total");
    let mut out = BTreeMap::new();
    for s in samples {
        if s.name == name || s.name == total {
            *out.entry(key(&s.labels)).or_default() += s.value;
        }
    }
    out
}

/// Sum of per-replica deltas of every histogram series.
pub fn histogram_deltas(
    before: &[Vec<Sample>],
    after: &[Vec<Sample>],
    metric: &str,
) -> BTreeMap<String, Histogram> {
    let mut out: BTreeMap<String, Histogram> = BTreeMap::new();
    for (i, after) in after.iter().enumerate() {
        let previous = before
            .get(i)
            .map(|b| histograms(b, metric))
            .unwrap_or_default();
        for (series, h) in histograms(after, metric) {
            let delta = h.delta(previous.get(&series));
            out.entry(series).or_default().merge(&delta);
        }
    }
    out
}

/// Sum of per-replica deltas of every counter series.
pub fn counter_deltas(
    before: &[Vec<Sample>],
    after: &[Vec<Sample>],
    name: &str,
) -> BTreeMap<String, f64> {
    let mut out: BTreeMap<String, f64> = BTreeMap::new();
    for (i, after) in after.iter().enumerate() {
        let previous = before.get(i).map(|b| counters(b, name)).unwrap_or_default();
        for (series, value) in counters(after, name) {
            let delta = (value - previous.get(&series).copied().unwrap_or(0.0)).max(0.0);
            if delta > 0.0 {
                *out.entry(series).or_default() += delta;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"# HELP gateway_admission_seconds x
# TYPE gateway_admission_seconds histogram
gateway_admission_seconds_bucket{le="0.001",phase="total",outcome="admitted"} 1
gateway_admission_seconds_bucket{le="0.002",phase="total",outcome="admitted"} 1
gateway_admission_seconds_bucket{le="+Inf",phase="total",outcome="admitted"} 1
gateway_admission_seconds_sum{phase="total",outcome="admitted"} 0.0005
gateway_admission_seconds_count{phase="total",outcome="admitted"} 1
gateway_settlements_total{outcome="settled"} 3
gateway_inference_attempts_total{provider="p",model="a,b \"c\"",outcome="succeeded"} 2
# EOF
"#;
    const AFTER: &str = r#"gateway_admission_seconds_bucket{le="0.001",phase="total",outcome="admitted"} 51
gateway_admission_seconds_bucket{le="0.002",phase="total",outcome="admitted"} 101
gateway_admission_seconds_bucket{le="+Inf",phase="total",outcome="admitted"} 101
gateway_admission_seconds_sum{phase="total",outcome="admitted"} 0.1255
gateway_admission_seconds_count{phase="total",outcome="admitted"} 101
gateway_settlements_total{outcome="settled"} 103
"#;

    #[test]
    fn parses_quoted_labels_and_values() {
        let samples = parse(BEFORE);
        let attempts = samples
            .iter()
            .find(|s| s.name == "gateway_inference_attempts_total")
            .unwrap();
        assert_eq!(attempts.labels[1], ("model".into(), "a,b \"c\"".into()));
        assert_eq!(attempts.value, 2.0);
        assert_eq!(samples.len(), 7);
    }

    #[test]
    fn deltas_merge_and_quantiles() {
        let before = vec![parse(BEFORE), parse(BEFORE)];
        let after = vec![parse(AFTER), parse(AFTER)];
        let h = histogram_deltas(&before, &after, "gateway_admission_seconds");
        let total = &h["phase=total,outcome=admitted"];
        assert_eq!(total.count, 200.0);
        assert_eq!(total.buckets[0], (0.001, 100.0));
        // Median: rank 100 of 200 is the top of the first bucket.
        assert!((total.quantile(0.5).unwrap() - 0.001).abs() < 1e-12);
        // p75: halfway through the second bucket.
        assert!((total.quantile(0.75).unwrap() - 0.0015).abs() < 1e-12);
        let summary = total.summary();
        assert_eq!(summary.count, 200);
        assert_eq!(summary.mean_ms, 1.25);
        let settled = counter_deltas(&before, &after, "gateway_settlements");
        assert_eq!(settled["outcome=settled"], 200.0);
        assert_eq!(Histogram::default().quantile(0.5), None);
    }

    #[test]
    fn infinite_bucket_reports_the_largest_finite_bound() {
        let h = Histogram {
            buckets: vec![(0.1, 1.0), (f64::INFINITY, 10.0)],
            sum: 5.0,
            count: 10.0,
        };
        assert_eq!(h.quantile(0.99), Some(0.1));
    }
}
