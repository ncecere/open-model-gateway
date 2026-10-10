//! Secret randomness: inference key secrets, sign-in/session/CSRF tokens,
//! invitation tokens, file-store data keys and nonces.
//!
//! Every secret comes from the operating system's CSPRNG (`rand::rngs::SysRng`,
//! getrandom). Since rand 0.10 that source is fallible, and so is this module:
//! a failure is an error the caller must return (management `503`, sign-in
//! `500`, file store `unavailable`, CLI non-zero exit). It never panics and
//! never falls back to another generator: `rand::rng()`/`ThreadRng` is also a
//! CSPRNG but panics when the OS source fails, and seeded generators
//! (`StdRng`) are deterministic. The production dependency does not enable
//! rand's `thread_rng` feature; `StdRng` is used only by routing for its
//! seeded, non-secret weighted choice.
use std::fmt;

use rand::{TryRng, rngs::SysRng};

/// The operating system's random number generator failed. No secret was
/// produced; the operation must fail closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntropyUnavailable;

impl fmt::Display for EntropyUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the operating system random number generator is unavailable")
    }
}
impl std::error::Error for EntropyUnavailable {}

/// Fill `dest` from the OS CSPRNG, or fail closed (`dest` must then be
/// discarded).
pub fn fill(dest: &mut [u8]) -> Result<(), EntropyUnavailable> {
    #[cfg(any(test, feature = "integration-tests"))]
    if seam::failing() {
        return Err(failed());
    }
    SysRng.try_fill_bytes(dest).map_err(|_| failed())
}

/// `N` bytes from the OS CSPRNG.
pub fn bytes<const N: usize>() -> Result<[u8; N], EntropyUnavailable> {
    let mut out = [0u8; N];
    fill(&mut out)?;
    Ok(out)
}

/// 32 random bytes, hex-encoded (64 characters).
pub fn hex_token() -> Result<String, EntropyUnavailable> {
    Ok(hex::encode(bytes::<32>()?))
}

fn failed() -> EntropyUnavailable {
    // No secret material exists to log; the error itself is the signal.
    tracing::error!(
        "operating system random number generator failed; refusing to generate a secret"
    );
    EntropyUnavailable
}

/// Test seam: make [`fill`] fail on the current thread while the guard lives
/// (tests run handlers on a current-thread runtime, so the failure stays
/// local to the test that asked for it).
#[cfg(any(test, feature = "integration-tests"))]
pub mod seam {
    use std::cell::Cell;

    thread_local! {
        static FAIL: Cell<bool> = const { Cell::new(false) };
    }
    pub(super) fn failing() -> bool {
        FAIL.with(Cell::get)
    }
    /// Until dropped, every [`super::fill`] on this thread fails.
    pub struct FailGuard(());
    impl Drop for FailGuard {
        fn drop(&mut self) {
            FAIL.with(|f| f.set(false));
        }
    }
    pub fn fail_on_this_thread() -> FailGuard {
        FAIL.with(|f| f.set(true));
        FailGuard(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_have_the_expected_length_and_differ() {
        let a = bytes::<32>().unwrap();
        let b = bytes::<32>().unwrap();
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
        let t = hex_token().unwrap();
        assert_eq!(t.len(), 64);
        assert!(t.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, hex_token().unwrap());
    }

    #[test]
    fn the_seam_fails_closed_and_resets() {
        {
            let _fail = seam::fail_on_this_thread();
            let mut buf = [7u8; 16];
            assert_eq!(fill(&mut buf), Err(EntropyUnavailable));
            assert_eq!(bytes::<8>(), Err(EntropyUnavailable));
            assert_eq!(hex_token(), Err(EntropyUnavailable));
        }
        assert!(bytes::<8>().is_ok());
    }

    /// Secrets are generated only here: no other production module uses
    /// `rand` (no ThreadRng, seeded or weaker generators for secrets).
    #[test]
    fn only_this_module_draws_randomness() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);
        let mut offenders = Vec::new();
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let test_file = name.ends_with("_tests.rs") || name == "tests.rs";
            // routing.rs: a seeded StdRng for deterministic, non-secret
            // weighted candidate order (never a secret).
            if test_file || path.ends_with("entropy.rs") || path.ends_with("routing.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Production code only; inline `mod tests` blocks come last.
            let production = text.split("#[cfg(test)]").next().unwrap_or_default();
            for needle in [
                "rand::",
                "OsRng",
                "SysRng",
                "thread_rng",
                "StdRng",
                "SmallRng",
            ] {
                if production.contains(needle) {
                    offenders.push(format!("{}: {needle}", path.display()));
                }
            }
        }
        assert!(offenders.is_empty(), "{offenders:#?}");
    }
}
