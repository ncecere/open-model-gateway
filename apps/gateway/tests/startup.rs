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
