//! Secret randomness: inference key secrets, sign-in/session/CSRF tokens,
//! invitation tokens, file-store data keys and nonces.
//!
//! Every secret comes from the operating system's CSPRNG (`rand::rngs::SysRng`,
//! getrandom). Since rand 0.10 that source is fallible, and so is this module:
//! a failure is an error the caller must return (management `503`, sign-in
//! `500`, file store `unavailable`, CLI non-zero exit). It never panics and
//! never falls back to another generator: `rand::rng()`/`ThreadRng` is also a
//! CSPRNG but panics when the OS source fails, and seeded generators
//! (`StdRng`) are deterministic. The gateway's own rand dependency does not
//! enable `thread_rng`, but tungstenite 0.30 (upstream realtime transport)
//! enables rand's default features, so `ThreadRng` is compiled in
//! transitively and used by tungstenite only for WebSocket masking and
//! handshake keys; no gateway secret is drawn from it. `StdRng` is used only
//! by routing for its seeded, non-secret weighted choice.
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
    fill_quiet(dest).map_err(|_| failed())
}

fn fill_quiet(dest: &mut [u8]) -> Result<(), EntropyUnavailable> {
    #[cfg(any(test, feature = "integration-tests"))]
    if seam::failing() {
        return Err(EntropyUnavailable);
    }
    SysRng.try_fill_bytes(dest).map_err(|_| EntropyUnavailable)
}

/// Non-secret optional telemetry identity: fail without logging or a fallback.
/// Secret generation must keep using `fill`/`bytes` and its diagnostics.
pub(crate) fn bytes_quiet<const N: usize>() -> Result<[u8; N], EntropyUnavailable> {
    let mut out = [0u8; N];
    fill_quiet(&mut out)?;
    Ok(out)
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
    use std::collections::BTreeSet;

    use proc_macro2::{Delimiter, TokenStream, TokenTree};
    use syn::parse::Parser;

    use super::*;
    use crate::configuration_source_tests::{production_sources, test_only};

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

    #[test]
    fn optional_identity_uses_the_same_fallible_source_without_a_fallback() {
        {
            let _fail = seam::fail_on_this_thread();
            assert_eq!(bytes_quiet::<16>(), Err(EntropyUnavailable));
        }
        assert!(bytes_quiet::<16>().is_ok());
    }

    /// Names that reach a random number generator: the `rand` crate, the
    /// crates under and beside it, and its generators by name.
    const NEEDLES: [&str; 10] = [
        "rand",
        "rand_core",
        "getrandom",
        "fastrand",
        "OsRng",
        "SysRng",
        "ThreadRng",
        "thread_rng",
        "StdRng",
        "SmallRng",
    ];

    /// The length in tokens of the test module that starts at `trees[0]`:
    /// outer attributes of which one is `#[cfg(test)]` or
    /// `#[cfg(all(test, ...))]` (`test_only`), an optional visibility, then
    /// `mod name;` or `mod name { ... }`. Anything else is not a test module.
    fn test_module(trees: &[TokenTree]) -> Option<usize> {
        let mut at = 0;
        let mut attributes = TokenStream::new();
        while let [TokenTree::Punct(hash), TokenTree::Group(group), ..] = &trees[at..]
            && hash.as_char() == '#'
            && group.delimiter() == Delimiter::Bracket
        {
            attributes.extend(trees[at..at + 2].iter().cloned());
            at += 2;
        }
        let attributes = syn::Attribute::parse_outer.parse2(attributes).ok()?;
        if !test_only(&attributes) {
            return None;
        }
        if matches!(trees.get(at), Some(TokenTree::Ident(word)) if word == "pub") {
            at += 1;
            if matches!(trees.get(at), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis)
            {
                at += 1;
            }
        }
        match &trees[at..] {
            [TokenTree::Ident(word), TokenTree::Ident(_), body, ..] if word == "mod" => {
                match body {
                    TokenTree::Punct(end) if end.as_char() == ';' => Some(at + 3),
                    TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                        Some(at + 3)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn scan(stream: TokenStream, found: &mut BTreeSet<&'static str>) {
        let trees: Vec<TokenTree> = stream.into_iter().collect();
        let mut at = 0;
        while at < trees.len() {
            if let Some(length) = test_module(&trees[at..]) {
                at += length;
                continue;
            }
            match &trees[at] {
                TokenTree::Group(group) => scan(group.stream(), found),
                TokenTree::Punct(_) => {}
                // An identifier, or a name inside a literal (a path in an
                // attribute string such as `#[serde(default = "...")]`).
                word => {
                    let word = word.to_string();
                    for word in word.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                        found.extend(NEEDLES.iter().filter(|needle| **needle == word));
                    }
                }
            }
            at += 1;
        }
    }

    /// The random-number entry points named by one source file outside its
    /// test modules (#506). Only a test module is left out, at any depth; a
    /// `#[cfg(test)]` on anything else (a `use`, a function, an impl) is
    /// scanned, and so is everything after it and after a test module. The
    /// file is read as tokens, so whatever is not recognised as a test module
    /// is scanned. `//` comments are not tokens; doc comments are attributes
    /// and count. A file that does not lex as Rust fails the test.
    fn draws(source: &str) -> Vec<&'static str> {
        let mut found = BTreeSet::new();
        scan(source.parse().expect("Rust source"), &mut found);
        found.into_iter().collect()
    }

    const DRAW: &str = "fn r() -> u8 { rand::random::<u8>() }\n";

    /// Only test modules are left out of the scan (#506): a `#[cfg(test)]` on
    /// anything else does not end the production code of a file.
    #[test]
    fn a_draw_after_an_early_test_only_item_is_reported() {
        for early in [
            "#[cfg(test)]\nuse std::fmt;\n",
            "#[cfg(test)]\n#[path = \"a/tests.rs\"]\nmod tests;\n",
            "#[cfg(all(test, feature = \"integration-tests\"))]\nmod gated;\n",
            "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n",
            "#[cfg(all(test, feature = \"integration-tests\"))]\nmod gated {\n    fn t() {}\n}\n",
            // Not a module, so the helper is scanned too, and so is what follows.
            "#[cfg(test)]\nfn helper() {}\n",
            // Built without `test` as well: production code.
            "#[cfg(any(test, feature = \"integration-tests\"))]\nmod seam {}\n",
        ] {
            let source = format!("fn a() {{}}\n{early}{DRAW}");
            assert_eq!(draws(&source), ["rand"], "{source}");
        }
    }

    #[test]
    fn a_draw_in_a_test_only_function_or_a_shared_module_is_reported() {
        for source in [
            format!("#[cfg(test)]\n{DRAW}"),
            format!("#[cfg(any(test, feature = \"integration-tests\"))]\nmod seam {{\n{DRAW}}}\n"),
            format!("mod inner {{\n{DRAW}}}\n"),
        ] {
            assert_eq!(draws(&source), ["rand"], "{source}");
        }
    }

    #[test]
    fn a_draw_inside_a_test_module_is_not_reported() {
        for source in [
            format!("fn a() {{}}\n#[cfg(test)]\nmod tests {{\n{DRAW}}}\n"),
            format!(
                "#[cfg(all(test, feature = \"integration-tests\"))]\n#[allow(dead_code)]\nmod x {{\n{DRAW}}}\nfn a() {{}}\n"
            ),
            format!("mod outer {{\n    #[cfg(test)]\n    mod tests {{\n{DRAW}    }}\n}}\n"),
        ] {
            assert!(draws(&source).is_empty(), "{source}");
        }
    }

    #[test]
    fn the_scan_reads_names_not_substrings_and_fails_closed() {
        for (source, expected) in [
            ("use getrandom::fill;", vec!["getrandom"]),
            ("use rand as r;", vec!["rand"]),
            ("fn a() { let _ = fastrand::u8(..); }", vec!["fastrand"]),
            (
                "fn a() { let _ = vec![rand::rngs::OsRng]; }",
                vec!["OsRng", "rand"],
            ),
            (
                "#[serde(default = \"rand::random\")]\nstruct A;",
                vec!["rand"],
            ),
            ("fn a() { rand_core::OsRng; }", vec!["OsRng", "rand_core"]),
            // Not a test module: scanned.
            ("#[cfg(not(test))]\nmod m { use rand::Rng; }", vec!["rand"]),
            (
                "#[cfg(test)]\nimpl A { fn a() { rand::rng(); } }",
                vec!["rand"],
            ),
            ("#[cfg(test)]\nmod m!{ rand }", vec!["rand"]),
            (
                "#[cfg(feature = \"test\")]\nmod m { use rand::Rng; }",
                vec!["rand"],
            ),
            // A test module, with a visibility or a doc comment.
            ("#[cfg(test)]\npub(crate) mod m { use rand::Rng; }", vec![]),
            (
                "/// Tests.\n#[cfg(test)]\npub mod m { use rand::Rng; }",
                vec![],
            ),
            // Other names, and `//` comments.
            (
                "// rand::random\nfn operand(random: u8, brand: u8) {}",
                vec![],
            ),
        ] {
            assert_eq!(draws(source), expected, "{source}");
        }
    }

    /// Secrets are generated only here: no other production module uses
    /// `rand` (no ThreadRng, seeded or weaker generators for secrets).
    #[test]
    fn only_this_module_draws_randomness() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut scanned = 0;
        // Every file that is not declared only under a test gate; a file name
        // does not make a file a test file.
        for source in production_sources() {
            // routing.rs: a seeded StdRng for deterministic, non-secret
            // weighted candidate order (never a secret).
            if matches!(source.name.as_str(), "entropy.rs" | "routing.rs") {
                continue;
            }
            let text = std::fs::read_to_string(root.join(&source.name)).unwrap();
            for needle in draws(&text) {
                offenders.push(format!("{}: {needle}", source.name));
            }
            scanned += 1;
        }
        assert!(
            scanned > 200,
            "scanned {scanned} files; did the scan break?"
        );
        assert!(
            offenders.is_empty(),
            "random number generators named outside entropy.rs (code, macros, attribute strings or doc comments): {offenders:#?}"
        );
    }
}
