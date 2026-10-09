//! Pure scheduling rules: time windows (overnight spans, DST boundaries),
//! the Prometheus reader, load thresholds and settings validation.
use chrono::TimeZone;

use super::*;

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}
fn window(tz: &str, days: &[&str], start: &str, end: &str) -> CompiledWindow {
    TimeWindow {
        timezone: tz.into(),
        days: days.iter().map(|d| (*d).to_owned()).collect(),
        start: start.into(),
        end: end.into(),
    }
    .compile()
    .unwrap()
}

#[test]
fn same_day_windows() {
    // Mon–Fri 09:00–17:00 UTC; 2025-06-02 is a Monday.
    let w = window(
        "UTC",
        &["mon", "tue", "wed", "thu", "fri"],
        "09:00",
        "17:00",
    );
    assert!(!w.allows(utc(2025, 6, 2, 8, 59)));
    assert!(w.allows(utc(2025, 6, 2, 9, 0)));
    assert!(w.allows(utc(2025, 6, 2, 16, 59)));
    assert!(!w.allows(utc(2025, 6, 2, 17, 0)));
    assert!(!w.allows(utc(2025, 6, 7, 12, 0)), "Saturday");
    // start == end: the whole day.
    let all = window("UTC", &["sun"], "00:00", "00:00");
    assert!(all.allows(utc(2025, 6, 8, 0, 0)));
    assert!(all.allows(utc(2025, 6, 8, 23, 59)));
    assert!(!all.allows(utc(2025, 6, 9, 0, 0)));
}

#[test]
fn overnight_spans_belong_to_the_day_they_start() {
    // Mon–Fri 19:00–07:00 America/New_York (EDT = UTC-4 in June).
    let w = window(
        "America/New_York",
        &["mon", "tue", "wed", "thu", "fri"],
        "19:00",
        "07:00",
    );
    // Monday 18:59 local: closed; 19:00: open.
    assert!(!w.allows(utc(2025, 6, 2, 22, 59)));
    assert!(w.allows(utc(2025, 6, 2, 23, 0)));
    // Tuesday 06:59 local (Monday's span): open; 07:00: closed.
    assert!(w.allows(utc(2025, 6, 3, 10, 59)));
    assert!(!w.allows(utc(2025, 6, 3, 11, 0)));
    // Friday's span runs into Saturday morning; Sunday's never starts, so
    // Monday morning is closed.
    assert!(w.allows(utc(2025, 6, 7, 10, 0)), "Saturday 06:00");
    assert!(!w.allows(utc(2025, 6, 7, 23, 30)), "Saturday 19:30");
    assert!(!w.allows(utc(2025, 6, 9, 10, 0)), "Monday 06:00");
}

#[test]
fn dst_boundaries_use_local_wall_clock() {
    // Saturday 19:00 → Sunday 07:00 in New York across both DST changes.
    let w = window("America/New_York", &["sat"], "19:00", "07:00");
    // Fall back (2025-11-02 02:00 EDT → 01:00 EST): the span lasts 13 h.
    assert!(w.allows(utc(2025, 11, 1, 23, 0)), "Sat 19:00 EDT");
    assert!(w.allows(utc(2025, 11, 2, 5, 30)), "01:30 EDT");
    assert!(
        w.allows(utc(2025, 11, 2, 6, 30)),
        "01:30 EST (repeated hour)"
    );
    assert!(w.allows(utc(2025, 11, 2, 11, 59)), "06:59 EST");
    assert!(!w.allows(utc(2025, 11, 2, 12, 0)), "07:00 EST");
    // Spring forward (2026-03-08 02:00 EST → 03:00 EDT): the span lasts 11 h.
    assert!(!w.allows(utc(2026, 3, 7, 23, 59)), "Sat 18:59 EST");
    assert!(w.allows(utc(2026, 3, 8, 0, 0)), "Sat 19:00 EST");
    assert!(w.allows(utc(2026, 3, 8, 10, 59)), "06:59 EDT");
    assert!(!w.allows(utc(2026, 3, 8, 11, 0)), "07:00 EDT");
    // A window inside the skipped hour never opens that night.
    let gap = window("America/New_York", &["sun"], "02:00", "03:00");
    assert!(!gap.allows(utc(2026, 3, 8, 7, 0)), "03:00 EDT");
    assert!(
        gap.allows(utc(2026, 3, 15, 6, 30)),
        "02:30 EDT a week later"
    );
}

#[test]
fn windows_reject_unknown_zones_days_and_times() {
    let ok = TimeWindow {
        timezone: "Europe/Berlin".into(),
        days: vec!["mon".into()],
        start: "22:00".into(),
        end: "06:00".into(),
    };
    assert!(ok.compile().is_some());
    for bad in [
        TimeWindow {
            timezone: "Mars/Olympus".into(),
            ..ok.clone()
        },
        TimeWindow {
            days: vec![],
            ..ok.clone()
        },
        TimeWindow {
            days: vec!["mon".into(), "mon".into()],
            ..ok.clone()
        },
        TimeWindow {
            days: vec!["monday".into()],
            ..ok.clone()
        },
        TimeWindow {
            start: "24:00".into(),
            ..ok.clone()
        },
        TimeWindow {
            end: "6:00".into(),
            ..ok.clone()
        },
    ] {
        assert!(bad.compile().is_none(), "{bad:?}");
    }
    assert_eq!(parse_hhmm("19:30"), Some(1170));
    assert_eq!(hhmm(1170), "19:30");
}

#[test]
fn prometheus_reader_takes_vllm_gauges() {
    let text = r#"
# HELP vllm:num_requests_running Number of requests in model execution batches.
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{engine="0",model_name="google/gemma-4"} 3.0
vllm:num_requests_running{engine="1",model_name="google/gemma-4"} 1.0
vllm:num_requests_waiting{engine="0",model_name="google/gemma-4"} 0.0
vllm:num_requests_waiting{engine="1",model_name="a \"quoted} name"} 2.0 1712345678
vllm:kv_cache_usage_perc{engine="0",model_name="google/gemma-4"} 0.42
vllm:kv_cache_usage_perc{engine="1",model_name="google/gemma-4"} 0.913
vllm:num_requests_waiting_by_reason{reason="capacity"} 99
vllm:prompt_tokens_total 1234
bogus line
vllm:num_requests_waiting{engine="2"} NaN
vllm:num_requests_waiting{engine="3"} -4
"#;
    assert_eq!(
        parse_metrics(text),
        Reading {
            waiting: Some(2),
            running: Some(4),
            kv_cache_permille: Some(913),
        }
    );
    // Older servers export gpu_cache_usage_perc; missing gauges are unknown.
    assert_eq!(
        parse_metrics("vllm:gpu_cache_usage_perc 0.5\n"),
        Reading {
            waiting: None,
            running: None,
            kv_cache_permille: Some(500),
        }
    );
}

#[test]
fn load_thresholds_pause_above_their_limit_and_fail_closed() {
    let gate = MetricsGate {
        url: "http://gpu.example:8000/metrics".into(),
        max_waiting: Some(0),
        max_running: None,
        max_kv_cache_percent: Some(90),
    };
    let reading = |waiting, kv| Reading {
        waiting,
        running: Some(8),
        kv_cache_permille: kv,
    };
    assert_eq!(gate.busy(&reading(Some(0), Some(900))), Ok(false));
    assert_eq!(gate.busy(&reading(Some(1), Some(100))), Ok(true));
    assert_eq!(gate.busy(&reading(Some(0), Some(901))), Ok(true));
    assert!(gate.busy(&reading(None, Some(100))).is_err());
    assert!(gate.busy(&reading(Some(0), None)).is_err());
    let running = MetricsGate {
        max_waiting: None,
        max_running: Some(8),
        max_kv_cache_percent: None,
        ..gate
    };
    assert_eq!(running.busy(&reading(None, None)), Ok(false));
}

#[test]
fn settings_validation() {
    let approvals = ApprovedEndpoints::for_test("http://127.0.0.1:8000/v1");
    let ok = RouteSettings {
        max_concurrency: 4,
        yield_live_threshold: Some(1),
        metrics: Some(MetricsGate {
            url: "http://127.0.0.1:8000/metrics".into(),
            max_waiting: Some(0),
            max_running: None,
            max_kv_cache_percent: Some(90),
        }),
        priority: Some(10),
        window: Some(TimeWindow {
            timezone: "America/New_York".into(),
            days: vec!["mon".into(), "fri".into()],
            start: "19:00".into(),
            end: "07:00".into(),
        }),
    };
    assert_eq!(ok.validate("vllm", &approvals), Ok(()));
    assert_eq!(
        RouteSettings::default().validate("openai", &approvals),
        Ok(())
    );
    // Priority hints only on vLLM-compatible routes, never negative.
    assert!(ok.validate("openai", &approvals).is_err());
    assert!(ok.validate("sglang", &approvals).is_err());
    let bad = |s: RouteSettings| s.validate("vllm", &approvals).is_err();
    assert!(bad(RouteSettings {
        priority: Some(-1),
        ..ok.clone()
    }));
    assert!(bad(RouteSettings {
        max_concurrency: 0,
        ..ok.clone()
    }));
    assert!(bad(RouteSettings {
        yield_live_threshold: Some(0),
        ..ok.clone()
    }));
    // Metrics only from an approved local origin, with a threshold.
    assert!(bad(RouteSettings {
        metrics: Some(MetricsGate {
            url: "http://169.254.169.254/metrics".into(),
            ..ok.metrics.clone().unwrap()
        }),
        ..ok.clone()
    }));
    assert!(bad(RouteSettings {
        metrics: Some(MetricsGate {
            max_waiting: None,
            max_kv_cache_percent: None,
            ..ok.metrics.clone().unwrap()
        }),
        ..ok.clone()
    }));
    // The API shape round-trips and refuses unknown fields.
    let v = serde_json::to_value(&ok).unwrap();
    assert_eq!(serde_json::from_value::<RouteSettings>(v).unwrap(), ok);
    assert!(
        serde_json::from_value::<RouteSettings>(json!({"max_concurrency":1,"surprise":true}))
            .is_err()
    );
}

#[test]
fn pause_codes_round_trip() {
    for p in [
        Pause::OutsideWindow,
        Pause::LiveTraffic,
        Pause::ServerBusy,
        Pause::MetricsUnavailable,
        Pause::Concurrency,
        Pause::FairShare,
        Pause::Workers,
        Pause::RateLimited,
    ] {
        assert_eq!(Pause::parse(p.as_str()), Some(p));
    }
    assert!(!Pause::MetricsUnavailable.legitimate());
    assert!(Pause::OutsideWindow.legitimate());
}
