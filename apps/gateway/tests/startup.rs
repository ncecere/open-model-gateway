//! The binary's own startup (the former container entrypoint) and its
//! `healthcheck` subcommand, run as a process with a clean environment.
//! No database, network or secrets beyond loopback fixtures.
use std::{
    ffi::OsStr,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output},
};

const BINARY: &str = env!("CARGO_BIN_EXE_open-model-gateway");

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn secret(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Never inherit the developer's environment or load the repository `.env`.
    fn run<I, K, V>(&self, args: &[&str], env: I) -> Output
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let empty_env_file = self.secret("empty.env", "");
        Command::new(BINARY)
            .args(args)
            .env_clear()
            .env("GATEWAY_ENV_FILE", empty_env_file)
            .envs(env)
            .current_dir(self.dir.path())
            .output()
            .unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn refuses_ambiguous_secrets_before_configuration_without_disclosure() {
    let f = Fixture::new();
    let path = f.secret("database url", "postgres://file-secret-marker\n");
    let output = f.run(
        &["serve"],
        [
            ("DATABASE_URL", "postgres://direct-secret-marker".as_ref()),
            ("DATABASE_URL_FILE", path.as_os_str()),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("secret configuration: DATABASE_URL: set either the variable or its _FILE companion, not both"),
        "{stderr}"
    );
    for sensitive in [
        "file-secret-marker",
        "direct-secret-marker",
        path.to_str().unwrap(),
    ] {
        assert!(!stderr.contains(sensitive), "must not disclose {sensitive}");
    }
}

#[test]
fn refuses_invalid_file_secrets_for_every_subcommand() {
    let f = Fixture::new();
    let path = f.secret("key", "first-sensitive\nsecond-sensitive");
    for args in [&["migrate"][..], &["serve"], &["schema-version"], &[]] {
        let output = f.run(args, [("OPENAI_API_KEY_FILE", &path)]);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(text(&output.stdout), "");
        let stderr = text(&output.stderr);
        assert!(
            stderr.contains("secret configuration: OPENAI_API_KEY: secret must be a single line")
        );
        assert!(!stderr.contains("sensitive"));
    }
}

#[test]
fn reserved_allowlists_stop_before_database_access_without_disclosure() {
    let f = Fixture::new();
    let database = "postgres://gateway:private-database-marker@127.0.0.1:1/gateway";
    // Excludes schema-version, healthcheck, --version and archive verify:
    // those intentionally return before Config::from_env.
    for args in [
        &[][..],
        &["serve"],
        &["migrate"],
        &["wait-for-database", "--timeout-seconds", "1"],
        &["grants", "apply"],
        &["grants", "verify"],
        &["bootstrap-dev"],
        &["bootstrap-demo"],
        &["reconcile-executions"],
        &["compact-history", "--older-than-days", "30"],
        &["alerts", "evaluate", "--once"],
        &["budget", "verify"],
        &["history", "verify"],
        &["partitions", "prepare"],
        &["partitions", "ensure"],
        &["partitions", "status"],
        &[
            "archive",
            "partition",
            "history",
            "2026-01",
            "--to",
            "unused",
        ],
        &["rollups", "run", "--once"],
        &["files", "verify"],
        &["files", "rekey", "--from", "old", "--to", "new"],
        &["files", "sweep", "--once"],
        &["provision-user", "--email", "fixture@example.test"],
    ] {
        for allowlist in [
            "GATEWAY_SECRET_ENV_ALLOWLIST",
            "GATEWAY_SMTP_SECRET_ENV_ALLOWLIST",
        ] {
            let mut cases = vec![
                ("DATABASE_URL", None),
                ("GATEWAY_OIDC_CLIENT_SECRET", None),
                ("AWS_SESSION_TOKEN", None),
                ("PGPASSWORD", None),
                ("PGSSLKEY", None),
                ("PGPASSFILE", None),
                ("PGFUTURE_OPTION", None),
                ("HTTP_PROXY", None),
                ("http_proxy", None),
                ("HTTPS_PROXY", None),
                ("https_proxy", None),
                ("ALL_PROXY", None),
                ("all_proxy", None),
                ("NO_PROXY", None),
                ("no_proxy", None),
                ("OTEL_EXPORTER_OTLP_HEADERS", None),
            ];
            for pointer in open_model_gateway::providers::secrets::SECRET_POINTERS {
                cases.push(("PRIVATE_MATERIAL", Some(*pointer)));
            }
            for (name, pointer) in cases {
                let mut env = vec![
                    ("DATABASE_URL", database),
                    (allowlist, name),
                    ("PRIVATE_MATERIAL", "private-value-marker"),
                ];
                if let Some(pointer) = pointer {
                    env.push((pointer, name));
                }
                let output = f.run(args, env);
                assert_eq!(output.status.code(), Some(1), "{args:?}");
                assert!(output.stdout.is_empty());
                let stderr = text(&output.stderr);
                // #99's newly inventoried pointer has stricter diagnostics:
                // setting only, not even the selected variable's name. Keep
                // every pre-existing pointer's assertions below unchanged.
                if pointer == Some("GATEWAY_METRICS_OTLP_HEADERS_ENV") {
                    assert!(stderr.contains("GATEWAY_METRICS_OTLP_HEADERS_ENV"));
                    for forbidden in [
                        name,
                        "could not connect",
                        "private-database-marker",
                        "private-value-marker",
                    ] {
                        assert!(!stderr.contains(forbidden), "{stderr}");
                    }
                    continue;
                }
                assert!(stderr.contains(allowlist));
                assert!(stderr.contains(name), "{args:?}");
                if let Some(pointer) = pointer {
                    assert!(stderr.contains(pointer));
                }
                assert!(!stderr.contains("could not connect"));
                assert!(!stderr.contains("private-database-marker"));
                assert!(!stderr.contains("private-value-marker"));
            }
        }
    }
}

#[test]
fn built_in_provider_keys_warn_at_startup_without_values_or_refusal() {
    let f = Fixture::new();
    let output = f.run(
        &["wait-for-database", "--timeout-seconds", "1"],
        [
            (
                "DATABASE_URL",
                "postgres://gateway:private-database-marker@127.0.0.1:1/gateway",
            ),
            (
                "GATEWAY_SECRET_ENV_ALLOWLIST",
                "OPENAI_API_KEY,ANTHROPIC_API_KEY",
            ),
        ],
    );
    let stderr = text(&output.stderr);
    let events: Vec<serde_json::Value> = stderr
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let warning = events
        .iter()
        .find(|event| {
            event["level"] == "WARN" && event["fields"]["setting"] == "GATEWAY_SECRET_ENV_ALLOWLIST"
        })
        .expect("startup must warn about provider keys");
    let names = warning["fields"]["names"].as_str().unwrap();
    assert!(names.contains("OPENAI_API_KEY"));
    assert!(names.contains("ANTHROPIC_API_KEY"));
    assert!(
        warning["fields"]["message"]
            .as_str()
            .unwrap()
            .contains("custom-endpoint")
    );
    assert!(!events.iter().any(|event| event["level"] == "ERROR"));
    assert!(!stderr.contains("reserved variables"));
    assert!(!stderr.contains("private-database-marker"));
}

#[test]
fn imports_a_file_secret_into_configuration() {
    let f = Fixture::new();
    // Without the import, configuration fails with "DATABASE_URL is required".
    let missing = f.run(&["serve"], std::iter::empty::<(&str, &str)>());
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing.stderr).contains("DATABASE_URL is required"));

    // Port 1 refuses connections: configuration accepted the imported URL.
    let path = f.secret(
        "database_url",
        "postgres://gateway:password-sensitive@127.0.0.1:1/gateway\n",
    );
    let output = f.run(&["serve"], [("DATABASE_URL_FILE", &path)]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("could not connect to PostgreSQL"),
        "{stderr}"
    );
    assert!(!stderr.contains("DATABASE_URL is required"));
    assert!(!stderr.contains("password-sensitive"));
}

#[test]
fn schema_version_and_version_need_no_database() {
    let f = Fixture::new();
    let output = f.run(&["schema-version"], std::iter::empty::<(&str, &str)>());
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("\"schema_family\":\"enterprise_v1\""));
    let output = f.run(&["--version"], std::iter::empty::<(&str, &str)>());
    assert_eq!(
        text(&output.stdout).trim(),
        format!("open-model-gateway {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn healthcheck_exit_status_follows_readiness() {
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for status in ["200 OK", "503 Service Unavailable"] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).unwrap();
            write!(stream, "HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").unwrap();
        }
    });
    // The default URL follows GATEWAY_LISTEN; an unreadable secret file is
    // irrelevant because healthcheck reads no secrets or configuration.
    let env = [
        ("GATEWAY_LISTEN", format!("0.0.0.0:{port}")),
        ("DATABASE_URL_FILE", "/nonexistent".into()),
    ];
    let healthy = f.run(&["healthcheck"], env.clone());
    assert_eq!(healthy.status.code(), Some(0), "{}", text(&healthy.stderr));
    assert_eq!(text(&healthy.stdout), "healthy (HTTP 200)\n");
    let unhealthy = f.run(&["healthcheck", "--timeout", "2s"], env);
    assert_eq!(unhealthy.status.code(), Some(1));
    assert!(text(&unhealthy.stderr).contains("HTTP 503"));
    server.join().unwrap();

    // Nothing listening; non-loopback refused; https not supported.
    for args in [
        vec![
            "healthcheck",
            "--url",
            &format!("http://127.0.0.1:{port}/health/ready"),
        ],
        vec!["healthcheck", "--url", "http://192.0.2.1:8080/health/ready"],
        vec![
            "healthcheck",
            "--url",
            "https://127.0.0.1:8443/health/ready",
        ],
    ] {
        let output = f.run(&args, std::iter::empty::<(&str, &str)>());
        assert_eq!(output.status.code(), Some(1), "{args:?}");
    }
}

/// #99 slice 1: config is validated before any DB access. Even valid metrics
/// export settings are refused explicitly until the worker ships (slice 3).
#[test]
fn metrics_otlp_refusals_use_the_real_startup_path_without_disclosure() {
    const URL: &str = "GATEWAY_METRICS_OTLP_URL";
    const HEADERS: &str = "GATEWAY_METRICS_OTLP_HEADERS_ENV";
    const INTERVAL: &str = "GATEWAY_METRICS_OTLP_INTERVAL_SECONDS";
    const TRAFFIC: &str = "GATEWAY_METRICS_OTLP_TRAFFIC";
    let f = Fixture::new();
    let valid = "https://collector.invalid/v1/metrics";
    let too_many = (0..17)
        .map(|i| format!("X-{i}=canary-secret"))
        .collect::<Vec<_>>()
        .join(",");
    let too_long = format!("X={}", "a".repeat(4095));
    let mut cases = vec![
        (HEADERS, vec![(URL, valid), (HEADERS, "PRIVATE_MATERIAL")]),
        (
            HEADERS,
            vec![
                (URL, valid),
                (HEADERS, "PRIVATE_MATERIAL"),
                ("PRIVATE_MATERIAL", "canary-no-equals"),
            ],
        ),
        (
            HEADERS,
            vec![
                (URL, valid),
                (HEADERS, "PRIVATE_MATERIAL"),
                ("PRIVATE_MATERIAL", too_many.as_str()),
            ],
        ),
        (
            HEADERS,
            vec![
                (URL, valid),
                (HEADERS, "PRIVATE_MATERIAL"),
                ("PRIVATE_MATERIAL", too_long.as_str()),
            ],
        ),
        (URL, vec![(URL, "https://10.1.1.1/v1/metrics")]),
        (
            URL,
            vec![
                (URL, "https://169.254.169.254/v1/metrics"),
                ("GATEWAY_EGRESS_PRIVATE_ALLOW", "0.0.0.0/0"),
            ],
        ),
        (URL, vec![(URL, "https://collector.invalid/v1/traces")]),
        (URL, vec![(URL, "https://collector.invalid/v1/logs")]),
        (
            URL,
            vec![(
                URL,
                "https://canary-user:canary-pass@collector.invalid/v1/metrics",
            )],
        ),
        (
            URL,
            vec![(URL, "https://collector.invalid/v1/metrics?canary-query")],
        ),
        (
            URL,
            vec![(URL, "https://collector.invalid/v1/metrics#canary-fragment")],
        ),
        (URL, vec![(URL, "")]),
        (URL, vec![(URL, "http://127.0.0.1:4318/v1/metrics")]),
        (INTERVAL, vec![(URL, valid), (INTERVAL, "9")]),
        (INTERVAL, vec![(URL, valid), (INTERVAL, "301")]),
        (INTERVAL, vec![(URL, valid), (INTERVAL, "60.0")]),
        (TRAFFIC, vec![(URL, valid), (TRAFFIC, "ON")]),
        (
            HEADERS,
            vec![
                (URL, valid),
                (HEADERS, "PRIVATE_MATERIAL"),
                ("PRIVATE_MATERIAL", "Host=canary-secret"),
            ],
        ),
        (
            HEADERS,
            vec![
                (URL, valid),
                (HEADERS, "PRIVATE_MATERIAL"),
                ("PRIVATE_MATERIAL", "X=canary-secret\r\n"),
            ],
        ),
        (
            HEADERS,
            vec![
                (URL, valid),
                (HEADERS, "PGPASSWORD"),
                ("PGPASSWORD", "X=canary-secret"),
            ],
        ),
        (
            HEADERS,
            vec![
                (URL, valid),
                (HEADERS, "PRIVATE_MATERIAL"),
                ("GATEWAY_SECRET_ENV_ALLOWLIST", "PRIVATE_MATERIAL"),
            ],
        ),
        (
            HEADERS,
            vec![
                (URL, valid),
                (HEADERS, "PRIVATE_MATERIAL"),
                ("GATEWAY_SMTP_SECRET_ENV_ALLOWLIST", "PRIVATE_MATERIAL"),
            ],
        ),
        (URL, vec![(URL, valid)]),
    ];
    for (setting, value) in [
        (HEADERS, "PRIVATE_MATERIAL"),
        (INTERVAL, "60"),
        (TRAFFIC, "off"),
    ] {
        cases.push((setting, vec![(setting, value)]));
    }
    for pointer in open_model_gateway::providers::secrets::SECRET_POINTERS {
        if *pointer != HEADERS {
            cases.push((
                HEADERS,
                vec![
                    (URL, valid),
                    (HEADERS, "PRIVATE_MATERIAL"),
                    (pointer, "PRIVATE_MATERIAL"),
                    ("PRIVATE_MATERIAL", "X=canary-secret"),
                ],
            ));
        }
    }
    for (setting, mut env) in cases {
        env.push((
            "DATABASE_URL",
            "postgres://gateway:never-dialed-marker@127.0.0.1:1/gateway",
        ));
        let output = f.run(&["serve"], env.clone());
        assert_eq!(output.status.code(), Some(1), "{setting}");
        assert!(output.stdout.is_empty());
        let stderr = text(&output.stderr);
        assert!(stderr.contains(setting), "{setting}: {stderr}");
        assert!(!stderr.contains("could not connect"), "{stderr}");
        for value in [
            "canary-",
            "never-dialed-marker",
            valid,
            "10.1.1.1",
            "169.254.169.254",
            "PRIVATE_MATERIAL",
        ] {
            assert!(!stderr.contains(value), "disclosure: {stderr}");
        }
        if env.contains(&(URL, valid)) && env.len() == 2 {
            assert!(stderr.contains("not implemented"), "{stderr}");
        }
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_metrics_settings_fail_real_startup() {
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new();
    for setting in [
        "GATEWAY_METRICS_OTLP_URL",
        "GATEWAY_METRICS_OTLP_HEADERS_ENV",
        "GATEWAY_METRICS_OTLP_INTERVAL_SECONDS",
        "GATEWAY_METRICS_OTLP_TRAFFIC",
    ] {
        let output = f.run(
            &["serve"],
            [
                (
                    std::ffi::OsString::from("DATABASE_URL"),
                    std::ffi::OsString::from("postgres://gateway@127.0.0.1:1/gateway"),
                ),
                (
                    setting.into(),
                    std::ffi::OsString::from_vec(b"canary-\xff".to_vec()),
                ),
            ],
        );
        assert_eq!(output.status.code(), Some(1));
        let stderr = text(&output.stderr);
        assert!(stderr.contains(setting), "{stderr}");
        assert!(!stderr.contains("canary"));
        assert!(!stderr.contains("could not connect"));
    }
}

/// #98 PR 3 acceptance: a refused trace-collector destination fails the
/// real binary's startup (`serve`, before any database connection is even
/// attempted - `Config::from_env` validates `GATEWAY_TRACES_*` ahead of
/// `connect()`) without ever echoing the destination, matching every other
/// reserved/secret refusal this file already proves. The unit-level
/// equivalent (`telemetry::tests::configuration_refuses_invalid_values_without_echoing_them`)
/// proves `TraceConfig::from_lookup` alone; this proves the same refusal
/// survives the real `main()` startup path end to end.
#[test]
fn a_refused_trace_collector_destination_fails_real_startup_without_echoing_it() {
    let f = Fixture::new();
    let database = "postgres://gateway:never-dialed-marker@127.0.0.1:1/gateway";
    for (case, url) in [
        // Private destination, production mode, no GATEWAY_EGRESS_PRIVATE_ALLOW.
        ("private", "https://10.1.1.1/v1/traces"),
        // Cloud metadata: refused in every mode, even with private allowed.
        ("metadata", "https://169.254.169.254/v1/traces"),
    ] {
        let mut env = vec![("DATABASE_URL", database), ("GATEWAY_TRACES_OTLP_URL", url)];
        if case == "metadata" {
            env.push(("GATEWAY_EGRESS_PRIVATE_ALLOW", "0.0.0.0/0"));
        }
        let output = f.run(&["serve"], env);
        assert_eq!(output.status.code(), Some(1), "{case}");
        assert!(output.stdout.is_empty(), "{case}");
        let stderr = text(&output.stderr);
        assert!(
            stderr.contains("GATEWAY_TRACES") || stderr.contains("trace"),
            "{case}: {stderr}"
        );
        assert!(
            !stderr.contains(url),
            "{case}: must never echo the refused URL: {stderr}"
        );
        assert!(
            !stderr.contains("could not connect"),
            "{case}: the database must never be dialed before trace config is validated: {stderr}"
        );
        assert!(!stderr.contains("never-dialed-marker"), "{case}");
    }
}
