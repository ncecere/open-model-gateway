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
//!
//! Two guards keep secrets here. The source scan in this module's tests
//! refuses generators by name (`rand`, `OsRng`, `RandomState`,
//! `/dev/urandom`, ...) outside this file. Library calls that draw randomness
//! without such a name (tungstenite's `generate_key`, `aead::Generate`,
//! openidconnect's `new_random` constructors, rustls's `SecureRandom::fill`,
//! `RandomState::new`) are `disallowed-methods` in `clippy.toml`, resolved by
//! path (#511). OAuth state, the OIDC nonce and the PKCE verifier are drawn
//! here and passed to the `new` constructors (`identity/login.rs`).
//!
//! `Uuid::new_v4()` and `Uuid::now_v7()` are allowed for identifiers only:
//! row ids, the public id half of a key token, lease and claim fencing
//! tokens compared in SQL, multipart boundaries and event ids. A value that
//! must be unguessable to protect anything (a credential, a token that
//! authorizes on its own, a key or a nonce) is drawn here, never from a UUID.
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

    use proc_macro2::{Delimiter, Literal, TokenStream, TokenTree};
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
    /// crates under and beside it, other generator crates, and generators by
    /// name (`thread_rng_n` is tokio's, public as
    /// `tokio::macros::support::thread_rng_n`). A name is matched whole, so
    /// each one is listed. The last three name what Clippy cannot refuse by
    /// path (#511): rustls's `CryptoProvider::secure_random` field, any
    /// `RandomState` (std's through `Default`, `ahash`, `foldhash`) and
    /// `/dev/urandom` (`/dev/random` is matched in [`names`]). Library calls
    /// that draw without any of these names are on the `clippy.toml` list
    /// ([`CLIPPY_LIST`]); `Uuid::new_v4()` is allowed for identifiers only.
    const NEEDLES: [&str; 20] = [
        "rand",
        "rand_core",
        "rand_chacha",
        "rand_pcg",
        "rand_xoshiro",
        "rand_distr",
        "getrandom",
        "fastrand",
        "nanorand",
        "oorandom",
        "OsRng",
        "SysRng",
        "ThreadRng",
        "thread_rng",
        "thread_rng_n",
        "StdRng",
        "SmallRng",
        "secure_random",
        "RandomState",
        "urandom",
    ];

    /// The length in tokens of the test module that starts at `trees[0]`:
    /// outer attributes of which one is `#[cfg(test)]` or
    /// `#[cfg(all(test, ...))]` (`test_only`), an optional visibility, then
    /// `mod name;` or `mod name { ... }`. Anything else is not a test module.
    fn test_module(trees: &[TokenTree]) -> Option<usize> {
        // Nearly every token: not an attribute, so not a test module.
        if !matches!(trees.first(), Some(TokenTree::Punct(hash)) if hash.as_char() == '#') {
            return None;
        }
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

    /// Whether a literal that could not be read spells an escape (`\x..` or
    /// `\u{..}`), which could hide a name from the scan of its source text.
    fn hides_an_escape(raw: &str) -> bool {
        raw.contains("\\x") || raw.contains("\\u")
    }

    /// The text of a literal in which names are looked for: the value of a
    /// string, byte string or C string, so that `"\x72and"` reads `rand`; the
    /// source text of any other literal. A literal that cannot be read is
    /// scanned as source text and must not hold an escape.
    fn literal_text(literal: &Literal) -> String {
        match syn::Lit::new(literal.clone()) {
            syn::Lit::Str(text) => text.value(),
            syn::Lit::ByteStr(text) => String::from_utf8_lossy(&text.value()).into_owned(),
            syn::Lit::CStr(text) => text.value().to_string_lossy().into_owned(),
            syn::Lit::Verbatim(raw) => {
                let raw = raw.to_string();
                assert!(
                    !hides_an_escape(&raw),
                    "a literal the scan cannot read holds an escape: {raw}"
                );
                raw
            }
            // Numbers, characters and booleans: one character names nothing.
            _ => literal.to_string(),
        }
    }

    fn names(text: &str, found: &mut BTreeSet<&'static str>) {
        for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            found.extend(NEEDLES.iter().filter(|needle| **needle == word));
        }
        // `random` alone is an ordinary word; only the device path counts.
        let device = text.split("/dev/").skip(1).any(|rest| {
            rest.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
                .is_some_and(|word| word == "random")
        });
        if device {
            found.insert("/dev/random");
        }
    }

    /// `module`: the tokens are the items of a module, that is the top of a
    /// file or the braces of a `mod name { }` that is itself at module level.
    /// Only there is a test module left out. Any other group (the body of a
    /// macro invocation or of a function, parentheses, brackets) is scanned
    /// in full, a `#[cfg(test)] mod` in it included: a macro can drop the
    /// attribute and keep the module.
    fn scan(stream: TokenStream, module: bool, found: &mut BTreeSet<&'static str>) {
        let trees: Vec<TokenTree> = stream.into_iter().collect();
        let mut at = 0;
        while at < trees.len() {
            if module && let Some(length) = test_module(&trees[at..]) {
                at += length;
                continue;
            }
            match &trees[at] {
                TokenTree::Group(group) => {
                    let items = module
                        && group.delimiter() == Delimiter::Brace
                        && at >= 2
                        && matches!(
                            &trees[at - 2..at],
                            [TokenTree::Ident(word), TokenTree::Ident(_)] if word == "mod"
                        );
                    scan(group.stream(), items, found);
                }
                TokenTree::Punct(_) => {}
                TokenTree::Ident(word) => names(&word.to_string(), found),
                // A name inside a literal: a path in an attribute string
                // such as `#[serde(default = "...")]`.
                TokenTree::Literal(literal) => names(&literal_text(literal), found),
            }
            at += 1;
        }
    }

    /// The random-number entry points named by one source file outside its
    /// test modules (#506). Only a test module is left out, at module level; a
    /// `#[cfg(test)]` on anything else (a `use`, a function, an impl) is
    /// scanned, and so is everything after it and after a test module. The
    /// file is read as tokens, so whatever is not recognised as a test module
    /// is scanned. `//` comments are not tokens; doc comments are attributes
    /// and count. A file that does not lex as Rust fails the test.
    fn draws(source: &str) -> Vec<&'static str> {
        let mut found = BTreeSet::new();
        scan(source.parse().expect("Rust source"), true, &mut found);
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

    /// Whether the scan reads the file `name` (a path under `src`).
    /// `production` is the shared inventory: every file that is not declared
    /// only under a test gate. A file outside it is left unread only if its
    /// name says it is a test file too (`tests.rs`, `*_tests.rs`, or under a
    /// `tests/` directory): the inventory follows `mod` declarations, and
    /// does not see a file pulled into production by `include!` or by a
    /// `cfg_attr` path (#510).
    fn read(name: &str, production: &BTreeSet<String>) -> bool {
        let mut parts = name.rsplit('/');
        let file = parts.next().unwrap_or_default();
        let test_name =
            file == "tests.rs" || file.ends_with("_tests.rs") || parts.any(|dir| dir == "tests");
        production.contains(name) || !test_name
    }

    /// The names `routing.rs` is reviewed to use: a seeded `StdRng` for
    /// deterministic, non-secret weighted candidate order (never a secret).
    const ROUTING: [&str; 2] = ["StdRng", "rand"];

    /// The needle names in every `pub use` / `pub(...) use` of `stream`, at
    /// any depth, test modules included.
    fn re_exports(stream: TokenStream, found: &mut BTreeSet<&'static str>) {
        let trees: Vec<TokenTree> = stream.into_iter().collect();
        for (at, tree) in trees.iter().enumerate() {
            if let TokenTree::Group(group) = tree {
                re_exports(group.stream(), found);
            }
            if !matches!(tree, TokenTree::Ident(word) if word == "pub") {
                continue;
            }
            let mut next = at + 1;
            if matches!(trees.get(next), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis)
            {
                next += 1;
            }
            if !matches!(trees.get(next), Some(TokenTree::Ident(word)) if word == "use") {
                continue;
            }
            let import = trees[next + 1..]
                .iter()
                .take_while(|tree| !matches!(tree, TokenTree::Punct(end) if end.as_char() == ';'))
                .cloned()
                .collect();
            scan(import, false, found);
        }
    }

    /// What is wrong with `routing.rs`: it names anything but exactly the
    /// reviewed set, or it re-exports a generator, which would hand it to
    /// other modules under a name this scan does not know.
    fn routing_problems(source: &str) -> Vec<String> {
        let mut problems = Vec::new();
        let named = draws(source);
        if named != ROUTING {
            problems.push(format!(
                "routing.rs: names {named:?}, reviewed for {ROUTING:?}"
            ));
        }
        let mut exported = BTreeSet::new();
        re_exports(source.parse().expect("Rust source"), &mut exported);
        problems.extend(
            exported
                .into_iter()
                .map(|name| format!("routing.rs: re-exports {name}")),
        );
        problems
    }

    /// A file declared only under a test gate is still read unless its name
    /// is a test name as well: `include!` or a `cfg_attr` path can pull such
    /// a file into production, which the declaration does not show (#506).
    #[test]
    fn only_a_file_that_is_a_test_by_declaration_and_by_name_is_not_read() {
        let production: BTreeSet<String> = ["alerts.rs", "harmless_tests.rs", "a/tests/used.rs"]
            .map(str::to_owned)
            .into_iter()
            .collect();
        for (name, expected) in [
            ("alerts.rs", true),
            // Declared in production: the name does not matter.
            ("harmless_tests.rs", true),
            ("a/tests/used.rs", true),
            // Declared test-only, but not a test name.
            ("authz_matrix.rs", true),
            ("providers/contract.rs", true),
            ("log_capture_test.rs", true),
            ("management_openapi/schema.rs", true),
            ("contests.rs", true),
            ("tests_helper/mod.rs", true),
            // Declared test-only and named as tests.
            ("tests.rs", false),
            ("identity/tests.rs", false),
            ("configuration_unsafe_tests.rs", false),
            ("scim/tests/users.rs", false),
            ("tests/payload.rs", false),
        ] {
            assert_eq!(read(name, &production), expected, "{name}");
        }
    }

    /// Library calls that draw randomness without naming a generator
    /// (#511). Clippy resolves these paths through aliases, re-exports and
    /// renamed imports, so `tokio_tungstenite::tungstenite::...` and
    /// `aes_gcm::aead::Generate` are covered by the defining crate's path.
    const CLIPPY_LIST: [&str; 30] = [
        "tungstenite::handshake::client::generate_key",
        "rustls::crypto::SecureRandom::fill",
        "crypto_common::Generate::generate",
        "crypto_common::Generate::try_generate",
        "crypto_common::Generate::generate_from_rng",
        "crypto_common::Generate::try_generate_from_rng",
        "crypto_common::KeyInit::generate_key",
        "crypto_common::KeyIvInit::generate_key",
        "crypto_common::KeyIvInit::generate_iv",
        "crypto_common::KeyIvInit::generate_key_iv",
        "oauth2::CsrfToken::new_random",
        "oauth2::CsrfToken::new_random_len",
        "oauth2::PkceCodeChallenge::new_random_sha256",
        "oauth2::PkceCodeChallenge::new_random_sha256_len",
        "oauth2::PkceCodeChallenge::new_random_plain",
        "openidconnect::Nonce::new_random",
        "openidconnect::Nonce::new_random_len",
        "std::hash::RandomState::new",
        "std::collections::HashMap::hasher",
        "std::collections::HashSet::hasher",
        "tower::util::rng::HasherRng::new",
        "tower::util::rng::Rng::next_u64",
        "tower::util::rng::Rng::next_f64",
        "tower::util::rng::Rng::next_range",
        "tungstenite::client::IntoClientRequest::into_client_request",
        "rustls::crypto::SupportedKxGroup::start",
        "rustls::crypto::SupportedKxGroup::start_and_complete",
        "rustls::crypto::ring::Ticketer::new",
        "rustls::crypto::aws_lc_rs::Ticketer::new",
        "rustls::server::ProducesTickets::encrypt",
    ];

    /// Every call on [`CLIPPY_LIST`] is a `disallowed-methods` entry in
    /// `clippy.toml` whose reason names this module (#511).
    #[test]
    fn clippy_disallows_randomness_that_names_no_generator() {
        let config = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("clippy.toml"),
        )
        .unwrap();
        for path in CLIPPY_LIST {
            let entry = format!("{{ path = \"{path}\", reason = \"");
            let line = config
                .lines()
                .find(|line| line.trim_start().starts_with(&entry))
                .unwrap_or_else(|| panic!("clippy.toml must disallow {path} (#511)"));
            assert!(
                line.contains("crate::entropy"),
                "clippy.toml: the reason for {path} must name crate::entropy (#511)"
            );
        }
    }

    /// What Clippy cannot name (a field, a type used through `Default`, a
    /// device path) is named here instead (#511).
    /// The crates whose paths `clippy.toml` names (#509, #511), at the
    /// versions the paths were written and probed for. Clippy reports a
    /// path it cannot resolve only as a warning, and not at all under
    /// `allow-invalid`, so a version change must bring the entries up for
    /// review: update the paths, re-run a planted probe, then this table.
    const REVIEWED_CRATES: [(&str, &[&str]); 8] = [
        ("crypto-common", &["0.1", "0.2"]),
        ("oauth2", &["5"]),
        ("openidconnect", &["4"]),
        ("reqwest", &["0.12"]),
        ("rustls", &["0.23"]),
        ("sqlx", &["0.8"]),
        ("tower", &["0.5"]),
        ("tungstenite", &["0.29", "0.30"]),
    ];

    /// `major.minor` for `0.x` versions, `major` otherwise (the semver
    /// compatibility line).
    fn line(version: &str) -> String {
        let mut parts = version.split('.');
        let major = parts.next().unwrap_or_default();
        if major == "0" {
            format!("0.{}", parts.next().unwrap_or_default())
        } else {
            major.to_owned()
        }
    }

    #[test]
    fn the_crates_clippy_toml_names_are_at_their_reviewed_versions() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock = std::fs::read_to_string(root.join("../../Cargo.lock")).unwrap();
        let mut versions: std::collections::BTreeMap<&str, BTreeSet<String>> = Default::default();
        let mut name = None;
        for text in lock.lines() {
            if let Some(value) = text.strip_prefix("name = ") {
                name = Some(value.trim_matches('"'));
            } else if let (Some(crate_name), Some(value)) =
                (name.take(), text.strip_prefix("version = "))
            {
                versions
                    .entry(crate_name)
                    .or_default()
                    .insert(line(value.trim_matches('"')));
            }
        }
        for (crate_name, reviewed) in REVIEWED_CRATES {
            let reviewed: BTreeSet<String> = reviewed.iter().map(|v| (*v).to_owned()).collect();
            assert_eq!(
                versions.get(crate_name),
                Some(&reviewed),
                "Cargo.lock: {crate_name} moved; review its clippy.toml entries (#511)"
            );
        }
        // Every crate an entry names is in the table.
        let config = std::fs::read_to_string(root.join("clippy.toml")).unwrap();
        for entry in config
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("{ path = \""))
        {
            let first = entry.split("::").next().unwrap().replace('_', "-");
            assert!(
                first == "std" || REVIEWED_CRATES.iter().any(|(c, _)| *c == first),
                "clippy.toml names {first}; add it to REVIEWED_CRATES (#511)"
            );
        }
    }

    #[test]
    fn sources_clippy_cannot_name_are_named() {
        for (source, expected) in [
            (
                "fn a() { let _ = std::fs::read(\"/dev/urandom\"); }",
                vec!["urandom"],
            ),
            ("const P: &[u8] = b\"/dev/urandom\";", vec!["urandom"]),
            (
                "fn a() { let _ = std::fs::File::open(\"/dev/random\"); }",
                vec!["/dev/random"],
            ),
            (
                "fn a(p: &rustls::crypto::CryptoProvider) { let _ = p.secure_random; }",
                vec!["secure_random"],
            ),
            (
                "use std::collections::hash_map::RandomState;",
                vec!["RandomState"],
            ),
            (
                "fn a() { let _ = <ahash::RandomState as Default>::default(); }",
                vec!["RandomState"],
            ),
            (
                "fn a() { let _ = tower::util::rng::HasherRng::default(); }",
                vec!["HasherRng"],
            ),
            // A path assembled by `concat!` or spelled with `.` and `//`.
            (
                "fn a() { let _ = std::fs::read(concat!(\"/dev/ur\", \"andom\")); }",
                vec!["urandom"],
            ),
            (
                "const P: &str = std::concat!(\"/dev\", concat!(\"/ran\", \"dom\"));",
                vec!["/dev/random"],
            ),
            (
                "fn a() { let _ = std::fs::read(\"/dev/./random\"); }",
                vec!["/dev/random"],
            ),
            (
                "fn a() { let _ = std::fs::read(\"/dev//random\"); }",
                vec!["/dev/random"],
            ),
            (
                "fn a() { let _ = std::fs::read(\"/tmp/../dev/random\"); }",
                vec!["/dev/random"],
            ),
            (
                "fn a() { let _ = std::fs::read(\"/proc/sys/kernel/random/uuid\"); }",
                vec!["/proc/sys/kernel/random"],
            ),
            (
                "fn a() { let _ = std::fs::read(\"/proc//sys/./kernel/random/boot_id\"); }",
                vec!["/proc/sys/kernel/random"],
            ),
            // pgcrypto's CSPRNG, in SQL.
            (
                "const Q: &str = \"SELECT encode(gen_random_bytes(32),'hex')\";",
                vec!["gen_random_bytes"],
            ),
        ] {
            assert_eq!(draws(source), expected, "{source}");
        }
        for source in [
            "fn a() { let _ = std::fs::read(\"/dev/null\"); }",
            "/// A random, opaque value.\nfn a() {}",
            "fn a() { let _ = \"/dev/randomness\"; }",
            "fn a() { let _ = concat!(\"/dev/\", \"null\"); }",
            "/// Ids come from `/proc`-free sources.\nfn a() {}",
            // Row ids: identifiers only, by policy.
            "const Q: &str = \"INSERT INTO t(id) VALUES(gen_random_uuid())\";",
        ] {
            assert!(draws(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn generators_reached_through_other_crates_are_named() {
        for (source, expected) in [
            (
                "fn x() { let _ = tokio::macros::support::thread_rng_n(4); }",
                vec!["thread_rng_n"],
            ),
            ("use rand_chacha::ChaCha20Rng;", vec!["rand_chacha"]),
            ("use rand_pcg::Pcg64;", vec!["rand_pcg"]),
            (
                "use rand_xoshiro::Xoshiro256PlusPlus;",
                vec!["rand_xoshiro"],
            ),
            ("use rand_distr::Normal;", vec!["rand_distr"]),
            ("fn a() { nanorand::tls_rng(); }", vec!["nanorand"]),
            ("fn a() { oorandom::Rand32::new(1); }", vec!["oorandom"]),
        ] {
            assert_eq!(draws(source), expected, "{source}");
        }
    }

    /// A macro can drop the attribute of a module in its body, so a test
    /// module is left out only where the compiler reads it as a module: at
    /// the top of a file or directly inside `mod name { }` (#506).
    #[test]
    fn a_test_module_is_left_out_only_at_module_level() {
        const MODULE: &str = "#[cfg(test)] mod x { fn r() -> u8 { rand::random::<u8>() } }";
        for source in [
            format!("keep! {{ {MODULE} }}"),
            format!("keep!({MODULE});"),
            format!("keep![{MODULE}];"),
            format!("fn f() {{ {MODULE} }}"),
            format!("impl A {{ fn f() {{ {MODULE} }} }}"),
            format!("mod outer {{ keep! {{ {MODULE} }} }}"),
            format!("keep! {{ mod outer {{ {MODULE} }} }}"),
            format!("macro_rules! m {{ () => {{ {MODULE} }}; }}"),
            format!("const A: u8 = {{ {MODULE} 1 }};"),
        ] {
            assert_eq!(draws(&source), ["rand"], "{source}");
        }
        for source in [
            MODULE.to_owned(),
            format!("mod outer {{ {MODULE} }}"),
            format!("pub(crate) mod outer {{ mod inner {{ {MODULE} }} }}"),
            format!("#[cfg(feature = \"a\")] mod outer {{ {MODULE} }}"),
        ] {
            assert!(draws(&source).is_empty(), "{source}");
        }
    }

    /// The value of a string is scanned, not its spelling (#506).
    #[test]
    fn a_name_spelled_with_escapes_in_a_literal_is_reported() {
        for source in [
            r#"#[serde(default = "\x72and::random")] struct A;"#,
            r#"#[serde(default = "\u{72}and::random")] struct A;"#,
            r#"#[serde(default = "\u{0072}\u{61}nd::random")] struct A;"#,
            "#[serde(default = \"ra\\\n    nd::random\")] struct A;",
            r#"#[serde(default = "\nrand::random")] struct A;"#,
            r##"#[serde(default = r#"rand::random"#)] struct A;"##,
            r#"const A: &[u8] = b"\x72and::random";"#,
            r#"const A: &core::ffi::CStr = c"\x72and::random";"#,
        ] {
            assert_eq!(draws(source), ["rand"], "{source}");
        }
        // No escape in a raw string: these are other names.
        assert!(draws(r#"const A: &str = r"\x72and \u{72}and";"#).is_empty());
        assert!(draws("const A: (u8, char, f32) = (0x72, '\\x72', 1.5e3);").is_empty());
    }

    #[test]
    fn a_literal_that_cannot_be_read_must_not_hold_an_escape() {
        for raw in [r"\x72and", r"x\u{72}and", r"(/*\x72*/)"] {
            assert!(hides_an_escape(raw), "{raw}");
        }
        for raw in ["(/*ERROR*/)", r"a\nb", "rand"] {
            assert!(!hides_an_escape(raw), "{raw}");
        }
    }

    /// `routing.rs` is read like any other file; it may name exactly the
    /// reviewed generator and must not hand it on (#506).
    #[test]
    fn routing_may_name_only_its_reviewed_generator_and_not_re_export_it() {
        const REVIEWED: &str = "use rand::{SeedableRng, rngs::StdRng};\nfn order(seed: [u8; 32]) { let _ = StdRng::from_seed(seed); }\n";
        assert_eq!(routing_problems(REVIEWED), Vec::<String>::new());
        for (extra, problem) in [
            ("use rand::rngs::SmallRng;\n", "SmallRng"),
            ("fn a() { let _ = rand::rngs::OsRng; }\n", "OsRng"),
            ("fn a() { fastrand::u8(..); }\n", "fastrand"),
            (
                "pub use rand::rngs::StdRng as Seeded;\n",
                "re-exports StdRng",
            ),
            ("pub(crate) use rand;\n", "re-exports rand"),
            (
                "pub(in crate::routing) use ::rand::{self as r};\n",
                "re-exports rand",
            ),
            ("mod a { pub(super) use rand::*; }\n", "re-exports rand"),
            (
                "keep! { pub use rand::rngs::{StdRng}; }\n",
                "re-exports StdRng",
            ),
        ] {
            let problems = routing_problems(&format!("{REVIEWED}{extra}"));
            assert!(
                problems.iter().any(|found| found.contains(problem)),
                "{extra}: {problems:?}"
            );
        }
        // A reviewed name that is gone is a change to review as well.
        assert!(!routing_problems("use rand::Rng;\n").is_empty());
        assert!(!routing_problems("fn a() {}\n").is_empty());
        // A private import and other public re-exports are fine.
        assert_eq!(
            routing_problems(&format!("{REVIEWED}pub use std::fmt;\nuse rand::Rng;\n")),
            Vec::<String>::new()
        );
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
        files.sort();
        let mut production: BTreeSet<String> = production_sources()
            .into_iter()
            .map(|source| source.name)
            .collect();
        let mut offenders = Vec::new();
        let mut scanned = 0;
        for path in files {
            let name = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let declared = production.remove(&name);
            // entropy.rs is the module. A file is otherwise left unread only
            // if it is a test file by declaration and by name.
            if name == "entropy.rs" || !(declared || read(&name, &production)) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            scanned += 1;
            if name == "routing.rs" {
                offenders.extend(routing_problems(&text));
                continue;
            }
            for needle in draws(&text) {
                offenders.push(format!("{name}: {needle}"));
            }
        }
        assert!(
            production.is_empty(),
            "inventory files the scan did not find: {production:?}"
        );
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
