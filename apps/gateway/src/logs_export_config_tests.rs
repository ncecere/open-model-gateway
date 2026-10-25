use super::{Environment, LogsExportConfig as C};

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
const URL: &str = "https://collector.invalid/v1/logs";

#[test]
fn disabled_by_default_and_warn_floor_by_default() {
    let disabled = parse(&[]).unwrap();
    assert!(disabled.endpoint.is_none());
    for value in ["info", "warn", "error"] {
        let config = parse(&[(C::URL, URL), (C::MIN_SEVERITY, value)]).unwrap();
        assert!(config.endpoint.is_some());
        assert_eq!(
            config.min_severity,
            crate::ops_log::Severity::parse(value).unwrap()
        );
    }
    let default_floor = parse(&[(C::URL, URL)]).unwrap();
    assert_eq!(default_floor.min_severity, crate::ops_log::Severity::Warn);
    for url in [
        "http://127.0.0.1:4318/v1/logs",
        "http://[::1]:4318/v1/logs",
        "http://localhost:4318/v1/logs",
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
fn invalid_settings_without_a_url_or_with_a_bad_value_refuse_startup() {
    for (setting, values) in [
        (C::URL, vec![(C::URL, "canary-invalid")]),
        (
            C::URL,
            vec![(C::URL, "https://collector.invalid/v1/traces")],
        ),
        (
            C::URL,
            vec![(C::URL, "https://collector.invalid/v1/metrics")],
        ),
        (
            C::URL,
            vec![(
                C::URL,
                "https://user:canary-password@collector.invalid/v1/logs",
            )],
        ),
        (
            C::URL,
            vec![(C::URL, "https://collector.invalid/v1/logs?canary-query")],
        ),
        (
            C::URL,
            vec![(C::URL, "https://collector.invalid/v1/logs#canary-fragment")],
        ),
        (C::URL, vec![(C::URL, "http://collector.invalid/v1/logs")]),
        (C::URL, vec![(C::URL, "https://169.254.169.254/v1/logs")]),
        (
            C::MIN_SEVERITY,
            vec![(C::URL, URL), (C::MIN_SEVERITY, "INFO")],
        ),
        (C::MIN_SEVERITY, vec![(C::URL, URL), (C::MIN_SEVERITY, "")]),
        (C::MIN_SEVERITY, vec![(C::MIN_SEVERITY, "warn")]),
        (C::HEADERS, vec![(C::HEADERS, "LOG_MATERIAL")]),
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
fn refuses_normalized_equality_with_the_active_audit_otlp_destination() {
    let error = C::from_lookup(
        |key| match key {
            k if k == C::URL => Some(URL.into()),
            "GATEWAY_AUDIT_OTLP_URL" => Some(URL.into()),
            _ => None,
        },
        Environment::Development,
    )
    .err()
    .expect("must refuse a collector alias with the audit pipeline")
    .to_string();
    assert!(error.contains(C::URL));
    assert!(error.contains("GATEWAY_AUDIT_OTLP_URL"));
    // A different logs destination from the audit one is unaffected.
    assert!(
        C::from_lookup(
            |key| match key {
                k if k == C::URL => Some(URL.into()),
                "GATEWAY_AUDIT_OTLP_URL" => Some("https://other-collector.invalid/v1/logs".into()),
                _ => None,
            },
            Environment::Development,
        )
        .is_ok()
    );
}

#[test]
fn headers_are_dedicated_bounded_sensitive_and_audit_style() {
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
                (C::HEADERS, "LOG_MATERIAL"),
                (pointer, " LOG_MATERIAL "),
                ("LOG_MATERIAL", "X=canary-secret"),
            ])
            .err()
            .unwrap()
            .to_string();
            assert!(error.contains(C::HEADERS));
            assert!(!error.contains("LOG_MATERIAL"));
        }
    }
    let config = parse(&[
        (C::URL, URL),
        (C::HEADERS, "LOG_MATERIAL"),
        ("LOG_MATERIAL", "Authorization=Bearer secret"),
    ])
    .unwrap();
    assert_eq!(config.headers["authorization"], "Bearer secret");
    assert!(config.headers.values().all(|value| value.is_sensitive()));
}
