//! Process startup hardening, run by `main` before configuration is loaded and
//! before any thread exists.
//!
//! This replaces the former `deploy/container-entrypoint.sh`, so the container
//! image needs no shell: `ENTRYPOINT ["/usr/local/bin/open-model-gateway"]`.
//! The behaviour is the same in and outside a container:
//!
//! - The process umask is `077`, so anything the gateway creates is private.
//! - Exactly four well-known secrets may be supplied through a file named by a
//!   `<NAME>_FILE` companion (Docker/Compose/Kubernetes secret mounts):
//!   [`SECRET_NAMES`]. Arbitrary `*_FILE` variables and custom provider
//!   references (`GATEWAY_SECRET_ENV_ALLOWLIST`) are never interpreted; inject
//!   those directly.
//! - Setting both a variable and its `_FILE` companion (even to an empty value)
//!   is refused. A file must be a readable regular file; at most 4097 bytes are
//!   read; one terminal LF is accepted. Every imported or directly supplied
//!   value among the four must be non-empty, at most 4096 bytes and a single
//!   line without CR, LF or NUL. On success the value is exported and the
//!   `_FILE` variable removed.
//! - Diagnostics contain only the fixed variable name and a fixed reason, never
//!   a value or a file path. Nothing here traces or logs.
//! - Nothing here migrates or bootstraps: the selected subcommand (default
//!   `serve`) runs exactly as given.
//!
//! The shell version forced `LC_ALL=C` so `${#value}` counted bytes; Rust
//! measures bytes natively, so no locale is involved.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    io::Read,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::Path,
};

use zeroize::Zeroize;

/// The only variables with `_FILE` indirection.
pub const SECRET_NAMES: [&str; 4] = [
    "DATABASE_URL",
    "GATEWAY_OIDC_CLIENT_SECRET",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
];

/// Maximum secret size in bytes (not characters).
pub const MAX_SECRET_BYTES: usize = 4096;

const AMBIGUOUS: &str = "set either the variable or its _FILE companion, not both";
const NOT_A_FILE: &str = "secret file must be a readable regular file";
const UNREADABLE: &str = "could not read secret file";
const TOO_LARGE: &str = "secret exceeds 4096 bytes";
const EMPTY: &str = "secret must not be empty";
const MULTILINE: &str = "secret must be a single line without CR or NUL bytes";

/// A refused secret. Holds only a fixed name and a fixed reason, so it can be
/// printed safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretError {
    pub name: &'static str,
    pub reason: &'static str,
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "secret configuration: {}: {}", self.name, self.reason)
    }
}

impl std::error::Error for SecretError {}

/// One validated secret to export.
pub struct SecretImport {
    pub name: &'static str,
    pub value: OsString,
    /// True when the value came from `<name>_FILE`, which is then removed.
    pub from_file: bool,
}

impl fmt::Debug for SecretImport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretImport")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("from_file", &self.from_file)
            .finish()
    }
}

impl Drop for SecretImport {
    fn drop(&mut self) {
        std::mem::take(&mut self.value).into_vec().zeroize();
    }
}

/// Resolve and validate the four secrets from `lookup` (the environment in
/// production). Reads secret files but changes nothing.
pub fn resolve_secrets(
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<Vec<SecretImport>, SecretError> {
    let mut imports = Vec::new();
    for name in SECRET_NAMES {
        let fail = |reason| SecretError { name, reason };
        let direct = lookup(name);
        let file = lookup(&format!("{name}_FILE"));
        let (mut value, from_file) = match (direct, file) {
            (Some(_), Some(_)) => return Err(fail(AMBIGUOUS)),
            (None, None) => continue,
            (Some(value), None) => (value.into_vec(), false),
            (None, Some(path)) => {
                let mut value = read_secret_file(&path).map_err(fail)?;
                if value.len() > MAX_SECRET_BYTES {
                    value.zeroize();
                    return Err(fail(TOO_LARGE));
                }
                // Accept one conventional terminal LF, never repeated LF or CRLF.
                if value.last() == Some(&b'\n') {
                    value.pop();
                }
                (value, true)
            }
        };
        if let Err(reason) = validate(&value) {
            value.zeroize();
            return Err(fail(reason));
        }
        imports.push(SecretImport {
            name,
            value: OsString::from_vec(value),
            from_file,
        });
    }
    Ok(imports)
}

fn validate(value: &[u8]) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err(EMPTY);
    }
    if value.len() > MAX_SECRET_BYTES {
        return Err(TOO_LARGE);
    }
    if value.iter().any(|b| matches!(b, b'\n' | b'\r' | 0)) {
        return Err(MULTILINE);
    }
    Ok(())
}

/// Read at most `MAX_SECRET_BYTES + 1` bytes from a readable regular file.
fn read_secret_file(path: &OsStr) -> Result<Vec<u8>, &'static str> {
    use std::os::unix::fs::OpenOptionsExt;
    if path.as_bytes().is_empty() {
        return Err(NOT_A_FILE);
    }
    let path = Path::new(path);
    // Follows symlinks (Kubernetes secret volumes use them). Checked before
    // opening so a FIFO or device is never opened; re-checked on the open
    // handle in case the path changed in between.
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return Err(NOT_A_FILE);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|_| NOT_A_FILE)?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err(NOT_A_FILE);
    }
    let mut value = Vec::with_capacity(MAX_SECRET_BYTES + 1);
    if file
        .take(MAX_SECRET_BYTES as u64 + 1)
        .read_to_end(&mut value)
        .is_err()
    {
        value.zeroize();
        return Err(UNREADABLE);
    }
    Ok(value)
}

/// Set the process umask to `077`.
pub fn restrict_umask() {
    // SAFETY: umask(2) has no memory-safety preconditions and cannot fail.
    unsafe {
        libc::umask(0o077);
    }
}

/// Validate the four secrets and export file-supplied values, removing their
/// `_FILE` variables. On error nothing is changed.
///
/// # Safety
///
/// Modifies the process environment: call only while the process is
/// single-threaded (before any runtime or thread is started), as required by
/// [`std::env::set_var`].
pub unsafe fn import_secret_files() -> Result<(), SecretError> {
    let imports = resolve_secrets(|name| std::env::var_os(name))?;
    for import in &imports {
        if import.from_file {
            // SAFETY: the caller guarantees the process is single-threaded.
            unsafe {
                std::env::set_var(import.name, &import.value);
                std::env::remove_var(format!("{}_FILE", import.name));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, os::unix::fs::PermissionsExt};

    struct Fixture {
        dir: tempfile::TempDir,
        env: HashMap<String, OsString>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                env: HashMap::new(),
            }
        }
        fn set(&mut self, name: &str, value: impl Into<OsString>) -> &mut Self {
            self.env.insert(name.into(), value.into());
            self
        }
        fn secret(&self, filename: &str, contents: &[u8]) -> OsString {
            let path = self.dir.path().join(filename);
            std::fs::write(&path, contents).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            path.into_os_string()
        }
        fn resolve(&self) -> Result<HashMap<&'static str, (Vec<u8>, bool)>, SecretError> {
            resolve_secrets(|name| self.env.get(name).cloned()).map(|imports| {
                imports
                    .iter()
                    .map(|i| (i.name, (i.value.as_bytes().to_vec(), i.from_file)))
                    .collect()
            })
        }
        fn rejected(&self, name: &'static str) -> SecretError {
            let error = self.resolve().expect_err("must be rejected");
            assert_eq!(error.name, name);
            error
        }
    }

    /// Diagnostics never contain values or paths: only fixed names/reasons.
    fn assert_safe(error: SecretError, sensitive: &[&[u8]]) {
        let text = error.to_string();
        assert!(text.starts_with(&format!("secret configuration: {}: ", error.name)));
        for value in sensitive {
            assert!(
                !text.as_bytes().windows(value.len()).any(|w| w == *value),
                "diagnostic must not disclose a value or path"
            );
        }
    }

    #[test]
    fn nothing_set_imports_nothing() {
        assert!(Fixture::new().resolve().unwrap().is_empty());
    }

    #[test]
    fn preserves_direct_secrets_literally() {
        let mut f = Fixture::new();
        let value = "  p@ss='quoted' \\ * $HOME ; $(touch x) `touch x`  ";
        for name in SECRET_NAMES {
            f.set(name, value);
        }
        let got = f.resolve().unwrap();
        for name in SECRET_NAMES {
            assert_eq!(got[name], (value.as_bytes().to_vec(), false));
        }
    }

    #[test]
    fn imports_all_four_file_secrets_preserving_spaces_and_one_final_lf() {
        let mut f = Fixture::new();
        let mut expected = HashMap::new();
        for (index, name) in SECRET_NAMES.into_iter().enumerate() {
            let value = format!("  literal-{index}:$HOME 'quotes' \\ ; *  ");
            let contents = format!("{value}{}", if index % 2 == 1 { "\n" } else { "" });
            let path = f.secret(&format!("{name} with spaces"), contents.as_bytes());
            f.set(&format!("{name}_FILE"), path);
            expected.insert(name, (value.into_bytes(), true));
        }
        assert_eq!(f.resolve().unwrap(), expected);
    }

    #[test]
    fn rejects_variable_and_file_ambiguity_even_when_empty() {
        for name in SECRET_NAMES {
            for direct in ["direct-secret-marker", ""] {
                let mut f = Fixture::new();
                let path = f.secret("secret", b"file-secret-marker");
                f.set(name, direct)
                    .set(&format!("{name}_FILE"), path.clone());
                let error = f.rejected(name);
                assert_eq!(error.reason, AMBIGUOUS);
                assert_safe(
                    error,
                    &[
                        path.as_bytes(),
                        b"file-secret-marker",
                        b"direct-secret-marker",
                    ],
                );
            }
        }
    }

    #[test]
    fn rejects_invalid_file_secrets_without_disclosure() {
        let oversized = vec![b's'; 4097];
        let cases: [(&str, &[u8]); 9] = [
            ("empty", b""),
            ("just a newline", b"\n"),
            ("multiline", b"first-sensitive\nsecond-sensitive"),
            ("repeated trailing newline", b"sensitive\n\n"),
            ("carriage return", b"sensitive\r"),
            ("CRLF", b"sensitive\r\n"),
            ("NUL", b"sensitive\0hidden"),
            ("terminal NUL", b"sensitive\0"),
            ("oversized", &oversized),
        ];
        for (label, contents) in cases {
            let mut f = Fixture::new();
            let path = f.secret("secret with spaces", contents);
            f.set("DATABASE_URL_FILE", path.clone());
            let error = f.resolve().expect_err(label);
            assert_eq!(error.name, "DATABASE_URL", "{label}");
            assert_safe(error, &[path.as_bytes(), b"sensitive", &[b's'; 100]]);
        }
    }

    #[test]
    fn rejects_invalid_direct_secrets_without_disclosure() {
        let oversized = "s".repeat(4097);
        for value in [
            "",
            "sensitive\nhidden",
            "sensitive\n",
            "sensitive\r",
            &oversized,
        ] {
            let mut f = Fixture::new();
            f.set("ANTHROPIC_API_KEY", value);
            assert_safe(
                f.rejected("ANTHROPIC_API_KEY"),
                &[b"sensitive", &[b's'; 100]],
            );
        }
    }

    #[test]
    fn enforces_a_4096_byte_limit_not_a_character_limit() {
        let mut f = Fixture::new();
        f.set("OPENAI_API_KEY", "é".repeat(2049));
        assert_eq!(f.rejected("OPENAI_API_KEY").reason, TOO_LARGE);

        let mut f = Fixture::new();
        let path = f.secret("secret", "é".repeat(2049).as_bytes());
        f.set("OPENAI_API_KEY_FILE", path);
        assert_eq!(f.rejected("OPENAI_API_KEY").reason, TOO_LARGE);

        let mut f = Fixture::new();
        f.set("OPENAI_API_KEY", "s".repeat(4096));
        assert_eq!(f.resolve().unwrap()["OPENAI_API_KEY"].0.len(), 4096);

        let mut f = Fixture::new();
        let path = f.secret("secret", &[b's'; 4096]);
        f.set("OPENAI_API_KEY_FILE", path);
        assert_eq!(f.resolve().unwrap()["OPENAI_API_KEY"].0.len(), 4096);

        // The limit applies before the terminal LF is removed, like the shell did.
        let mut f = Fixture::new();
        let path = f.secret("secret", &[&[b's'; 4096][..], b"\n"].concat());
        f.set("OPENAI_API_KEY_FILE", path);
        assert_eq!(f.rejected("OPENAI_API_KEY").reason, TOO_LARGE);
    }

    #[test]
    fn rejects_empty_missing_directory_and_special_file_paths() {
        let f = Fixture::new();
        let missing = f.dir.path().join("missing-sensitive-path").into_os_string();
        let directory = f.dir.path().as_os_str().to_owned();
        for path in [OsString::new(), missing, directory, "/dev/null".into()] {
            let mut f = Fixture::new();
            f.set("DATABASE_URL_FILE", path.clone());
            let error = f.rejected("DATABASE_URL");
            assert_eq!(error.reason, NOT_A_FILE);
            if !path.is_empty() {
                assert_safe(error, &[path.as_bytes()]);
            }
        }
    }

    #[test]
    fn follows_symlinks_like_kubernetes_secret_volumes() {
        let mut f = Fixture::new();
        let target = f.secret("..data-secret", b"postgres://fixture\n");
        let link = f.dir.path().join("database_url");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        f.set("DATABASE_URL_FILE", link);
        assert_eq!(
            f.resolve().unwrap()["DATABASE_URL"],
            (b"postgres://fixture".to_vec(), true)
        );
    }

    #[test]
    fn rejects_a_permission_denied_secret_file() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root bypasses Unix read permission checks
        }
        let mut f = Fixture::new();
        let path = f.secret("secret", b"private-sensitive-value");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        f.set("DATABASE_URL_FILE", path.clone());
        let error = f.rejected("DATABASE_URL");
        assert_eq!(error.reason, NOT_A_FILE);
        assert_safe(error, &[path.as_bytes(), b"private-sensitive-value"]);
    }

    #[test]
    fn ignores_custom_allowlist_names_and_arbitrary_file_variables() {
        let mut f = Fixture::new();
        let path = f.secret("secret", b"not-imported");
        f.set(
            "GATEWAY_SECRET_ENV_ALLOWLIST",
            "CUSTOM_PROVIDER_KEY,$(touch marker),BAD;NAME",
        )
        .set("CUSTOM_PROVIDER_KEY_FILE", path);
        assert!(f.resolve().unwrap().is_empty());
    }

    #[test]
    fn debug_output_is_redacted() {
        let mut f = Fixture::new();
        f.set("OPENAI_API_KEY", "sk-sensitive");
        let imports = resolve_secrets(|name| f.env.get(name).cloned()).unwrap();
        assert!(!format!("{imports:?}").contains("sk-sensitive"));
    }
}
