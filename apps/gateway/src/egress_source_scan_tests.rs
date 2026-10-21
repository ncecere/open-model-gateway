//! Source scans of the outbound HTTP clients (#196). They read this crate's
//! own source, so a client added or edited without the safeguards fails here.
//! The sibling scan `tests::every_http_client_uses_the_egress_resolver`
//! covers the address-policy resolver.
use std::path::{Path, PathBuf};

/// Every outbound HTTP client the gateway builds disables redirects (a
/// redirect would carry the request, and any credential, to a host nobody
/// approved), ambient proxies and implicit retries. The sibling scan in
/// `tests::every_http_client_uses_the_egress_resolver` covers the resolver.
#[test]
fn every_http_client_disables_redirects_proxies_and_retries() {
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
    let mut checked = 0;
    for path in all {
        let name = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if name.ends_with("tests.rs") || name.contains("_tests") || name.contains("/tests/") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
        for (at, _) in production.match_indices("Client::builder()") {
            let window: String = production[at..].chars().take(900).collect();
            let window = window.split(".build()").next().unwrap();
            for (needle, what) in [
                ("redirect(reqwest::redirect::Policy::none())", "redirects"),
                ("no_proxy()", "ambient proxies"),
                ("retry(reqwest::retry::never())", "implicit retries"),
            ] {
                // `resolve_to_addrs` clients (pinned local approvals) are the
                // one with their own address; everything else is covered too.
                assert!(
                    window.contains(needle),
                    "{name}: a client that does not disable {what}"
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 8, "found {checked} clients; did the scan break?");
}
