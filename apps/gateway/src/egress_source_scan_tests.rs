//! The egress client rule is enforced by Clippy (#509), not by reading
//! source: `apps/gateway/clippy.toml` lists every reqwest constructor and
//! every builder method that changes destination, proxy, redirect, retry,
//! DNS or TLS trust as `disallowed-methods`, and Clippy resolves them through
//! aliases, re-exports and renamed imports. These scans only keep that rule
//! switched on: the list stays complete, and production code never turns
//! the lint off except at the sanctioned sites below.
use std::path::{Path, PathBuf};

fn sources() -> Vec<(String, String)> {
    fn files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut all = Vec::new();
    files(&root, &mut all);
    all.into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (name, std::fs::read_to_string(&path).unwrap())
        })
        .collect()
}

/// Whitespace-free, so a lint path written with spaces around `::`, or an
/// attribute split over lines, reads the same.
fn compact(source: &str) -> String {
    source.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The lint's name is built here, so this file does not mention it.
fn lint() -> String {
    ["clippy", "disallowed_methods"].join("::")
}

/// Every file under `src/` (production and unit tests alike) names the lint
/// only at these sites. Unit tests need none: `lib.rs` allows the lint for
/// the whole test build, and the non-test build, which Clippy also checks,
/// contains all production code. Integration tests under `tests/` are their
/// own crates and opt out per file.
#[test]
fn only_the_sanctioned_sites_turn_off_disallowed_methods() {
    let lint = lint();
    // The old, renamed spelling still works as an alias; a prefix catches it.
    let prefix = lint.trim_end_matches('s').to_owned();
    let egress = format!("#[expect({lint},reason=\"theegress-guardedconstructor(#509)\")]");
    let sanctioned = [
        ("egress.rs", egress, 3),
        ("db.rs", format!("#[allow({lint})]"), 1),
        ("lib.rs", format!("#![cfg_attr(test,allow({lint}))]"), 1),
    ];
    let mut seen = 0;
    for (name, source) in sources() {
        let source = compact(&source);
        let named = source.matches(&prefix).count();
        let (allowed, count) = sanctioned
            .iter()
            .find(|(file, _, _)| *file == name)
            .map_or((0, 0), |(_, attribute, count)| {
                (source.matches(attribute.as_str()).count(), *count)
            });
        assert_eq!(
            (named, allowed),
            (count, count),
            "{name} names {lint} outside its sanctioned sites. Production code builds \
             HTTP clients with crate::egress::client_builder (#509)."
        );
        seen += named;
    }
    assert_eq!(
        seen, 5,
        "the scan found {seen} sanctioned sites; did it break?"
    );
}

/// Lint groups that contain `disallowed_methods`, and `warnings`, would turn
/// it off as well.
#[test]
fn no_lint_group_switches_disallowed_methods_off() {
    let mut checked = 0;
    for (name, source) in sources() {
        let source = compact(&source);
        for level in ["allow(", "expect("] {
            for (at, _) in source.match_indices(level) {
                // An attribute (`#[allow(`, `#![allow(`) or a cfg_attr branch.
                if !source[..at].ends_with('[') && !source[..at].ends_with(',') {
                    continue;
                }
                let body = &source[at + level.len()..];
                let body = &body[..body.find(')').unwrap_or(body.len())];
                for item in body.split(',') {
                    assert!(
                        !matches!(item, "warnings" | "clippy::all" | "clippy::style"),
                        "{name}: `{level}{item}` also switches off {} (#509)",
                        lint()
                    );
                }
                checked += 1;
            }
        }
    }
    assert!(
        checked > 100,
        "checked {checked} lint attributes; did the scan break?"
    );
}

/// Every way to build a reqwest client, or to change a safeguard that
/// `crate::egress::client_builder` sets, stays on the Clippy list and points
/// at the sanctioned constructor.
#[test]
fn clippy_disallows_every_unguarded_client_construction() {
    let config =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("clippy.toml")).unwrap();
    let required = [
        "reqwest::Client::new",
        "reqwest::Client::builder",
        "reqwest::ClientBuilder::new",
        "reqwest::get",
        "reqwest::ClientBuilder::redirect",
        "reqwest::ClientBuilder::retry",
        "reqwest::ClientBuilder::proxy",
        "reqwest::ClientBuilder::no_proxy",
        "reqwest::ClientBuilder::dns_resolver",
        "reqwest::ClientBuilder::dns_resolver2",
        "reqwest::ClientBuilder::resolve",
        "reqwest::ClientBuilder::resolve_to_addrs",
        "reqwest::ClientBuilder::connector_layer",
        "reqwest::ClientBuilder::unix_socket",
        "reqwest::ClientBuilder::danger_accept_invalid_certs",
        "reqwest::ClientBuilder::danger_accept_invalid_hostnames",
        "reqwest::ClientBuilder::use_preconfigured_tls",
        "reqwest::ClientBuilder::tls_built_in_root_certs",
        "reqwest::ClientBuilder::tls_built_in_webpki_certs",
        "reqwest::ClientBuilder::tls_built_in_native_certs",
        "reqwest::ClientBuilder::add_root_certificate",
        "reqwest::ClientBuilder::hickory_dns",
    ];
    for path in required {
        let entry = format!("{{ path = \"{path}\", reason = \"");
        let line = config
            .lines()
            .find(|line| line.trim_start().starts_with(&entry))
            .unwrap_or_else(|| panic!("clippy.toml must disallow {path} (#509)"));
        assert!(
            line.contains("crate::egress::"),
            "clippy.toml: the reason for {path} must name the egress constructor"
        );
    }
}
