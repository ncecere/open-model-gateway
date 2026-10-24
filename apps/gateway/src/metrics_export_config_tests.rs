use super::{Environment, MetricsExportConfig as C, TraceConfig};

fn parse(values: &[(&str, &str)]) -> anyhow::Result<C> {
    C::from_lookup(
        |key| {
            assert!(
                !key.starts_with("OTEL_"),
                "must never read standard OTEL settings"
            );
            values
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        },
        Environment::Development,
    )
}
const URL: &str = "https://collector.invalid/v1/metrics";

#[test]
fn metrics_config_defaults_bounds_and_independent_trace_settings() {
    let disabled = parse(&[]).unwrap();
    assert!(disabled.endpoint.is_none());
    assert_eq!(disabled.interval.as_secs(), 60);
    assert!(!disabled.traffic);
    for interval in ["10", "60", "300"] {
        for traffic in ["off", "on"] {
            let config = parse(&[
                (C::URL, URL),
                (C::INTERVAL, interval),
                (C::TRAFFIC, traffic),
            ])
            .unwrap();
            assert!(config.endpoint.is_some());
            assert!(config.headers.is_empty());
            assert_eq!(config.interval.as_secs(), interval.parse::<u64>().unwrap());
            assert_eq!(config.traffic, traffic == "on");
        }
    }
    assert!(!parse(&[(C::URL, URL)]).unwrap().traffic);
    assert!(
        parse(&[
            (TraceConfig::URL, "not-read"),
            (TraceConfig::RATIO, "not-read")
        ])
        .unwrap()
        .endpoint
        .is_none()
    );
    assert!(
        TraceConfig::from_lookup(
            |key| (key == C::URL).then(|| URL.into()),
            Environment::Development
        )
        .unwrap()
        .endpoint
        .is_none()
    );
    for url in [
        "http://127.0.0.1:4318/v1/metrics",
        "http://[::1]:4318/v1/metrics",
        "http://localhost:4318/v1/metrics",
    ] {
        assert!(parse(&[(C::URL, url)]).is_ok());
        assert!(
            C::from_lookup(
                |key| (key == C::URL).then(|| url.into()),
                Environment::Production
            )
            .is_err()
        );
    }
}

#[test]
fn metrics_config_rejects_invalid_settings_without_values() {
    for (setting, values) in [
        (C::URL, vec![(C::URL, "canary-invalid")]),
        (
            C::URL,
            vec![(C::URL, "https://collector.invalid/v1/traces")],
        ),
        (C::URL, vec![(C::URL, "https://collector.invalid/v1/logs")]),
        (
            C::URL,
            vec![(
                C::URL,
                "https://user:canary-password@collector.invalid/v1/metrics",
            )],
        ),
        (
            C::URL,
            vec![(C::URL, "https://collector.invalid/v1/metrics?canary-query")],
        ),
        (
            C::URL,
            vec![(
                C::URL,
                "https://collector.invalid/v1/metrics#canary-fragment",
            )],
        ),
        (
            C::URL,
            vec![(C::URL, "https://collector.invalid/v1/metrics\n")],
        ),
        (
            C::URL,
            vec![(C::URL, "http://collector.invalid/v1/metrics")],
        ),
        (C::URL, vec![(C::URL, "https://169.254.169.254/v1/metrics")]),
        (C::INTERVAL, vec![(C::URL, URL), (C::INTERVAL, "9")]),
        (C::INTERVAL, vec![(C::URL, URL), (C::INTERVAL, "301")]),
        (C::INTERVAL, vec![(C::URL, URL), (C::INTERVAL, "60.0")]),
        (C::INTERVAL, vec![(C::URL, URL), (C::INTERVAL, "-10")]),
        (C::INTERVAL, vec![(C::URL, URL), (C::INTERVAL, " 60")]),
        (C::INTERVAL, vec![(C::URL, URL), (C::INTERVAL, "+60")]),
        (C::TRAFFIC, vec![(C::URL, URL), (C::TRAFFIC, "ON")]),
        (C::TRAFFIC, vec![(C::URL, URL), (C::TRAFFIC, "")]),
        (C::HEADERS, vec![(C::HEADERS, "METRIC_MATERIAL")]),
        (C::INTERVAL, vec![(C::INTERVAL, "60")]),
        (C::TRAFFIC, vec![(C::TRAFFIC, "off")]),
        (
            C::HEADERS,
            vec![(C::URL, URL), (C::HEADERS, "UNSET_MATERIAL")],
        ),
    ] {
        let error = parse(&values).err().expect("must refuse").to_string();
        assert!(error.contains(setting), "{error}");
        assert!(!error.contains("canary"), "{error}");
        assert!(!error.contains("collector.invalid"), "{error}");
    }
}

#[test]
fn metrics_headers_are_dedicated_bounded_sensitive_and_audit_style() {
    for name in [
        "AWS_SESSION_TOKEN",
        "GATEWAY_SECRET",
        "DATABASE_URL",
        "OPENAI_API_KEY",
        "PGPASSWORD",
        "HTTP_PROXY",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "lowercase",
        "",
    ] {
        assert!(
            parse(&[(C::URL, URL), (C::HEADERS, name), (name, "X=canary-secret")]).is_err(),
            "{name}"
        );
    }
    for pointer in crate::providers::secrets::SECRET_POINTERS {
        if *pointer != C::HEADERS {
            let error = parse(&[
                (C::URL, URL),
                (C::HEADERS, "METRIC_MATERIAL"),
                (pointer, " METRIC_MATERIAL "),
                ("METRIC_MATERIAL", "X=canary-secret"),
            ])
            .err()
            .unwrap()
            .to_string();
            assert!(error.contains(C::HEADERS));
            assert!(!error.contains("METRIC_MATERIAL"));
        }
    }
    for allowlist in [
        "GATEWAY_SECRET_ENV_ALLOWLIST",
        "GATEWAY_SMTP_SECRET_ENV_ALLOWLIST",
    ] {
        assert!(
            parse(&[
                (C::URL, URL),
                (C::HEADERS, "METRIC_MATERIAL"),
                (allowlist, "OTHER, METRIC_MATERIAL"),
                ("METRIC_MATERIAL", "X=canary-secret")
            ])
            .is_err()
        );
    }
    let seventeen = (0..17)
        .map(|i| format!("X-{i}=canary-secret"))
        .collect::<Vec<_>>()
        .join(",");
    let oversized = format!("X={}", "a".repeat(4095));
    for value in [
        "",
        "canary-secret",
        "X=canary\rsecret",
        "X=canary\nsecret",
        "X=canary\tsecret",
        "Host=canary",
        "Content-Type=canary",
        "Content-Length=123",
        "Connection=canary",
        "Transfer-Encoding=chunked",
        seventeen.as_str(),
        oversized.as_str(),
    ] {
        let error = parse(&[
            (C::URL, URL),
            (C::HEADERS, "METRIC_MATERIAL"),
            ("METRIC_MATERIAL", value),
        ])
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains(C::HEADERS));
        assert!(!error.contains("canary"));
    }
    let config = parse(&[
        (C::URL, URL),
        (C::HEADERS, "METRIC_MATERIAL"),
        (
            "METRIC_MATERIAL",
            " Authorization = Bearer secret , X-Tenant = tenant ",
        ),
    ])
    .unwrap();
    assert_eq!(config.headers["authorization"], "Bearer secret");
    assert_eq!(config.headers["x-tenant"], "tenant");
    assert!(config.headers.values().all(|value| value.is_sensitive()));
    for value in [
        (0..16)
            .map(|i| format!("X-{i}=ok"))
            .collect::<Vec<_>>()
            .join(","),
        format!("X={}", "a".repeat(4094)),
    ] {
        assert!(
            parse(&[
                (C::URL, URL),
                (C::HEADERS, "METRIC_MATERIAL"),
                ("METRIC_MATERIAL", &value)
            ])
            .is_ok()
        );
    }
}
