//! The egress client rule is enforced by Clippy (#509), not by reading
//! source: `apps/gateway/clippy.toml` lists every reqwest constructor and
//! every builder method that changes destination, proxy, redirect, retry,
//! DNS or TLS trust as `disallowed-methods`, and Clippy resolves them through
//! aliases, re-exports and renamed imports. These tests only keep that rule
//! switched on: the list stays complete, nothing under `src/` turns the lint
//! off except the sanctioned sites, no file outside `src/` is pulled in as
//! code, and no Cargo or Clippy configuration switches it off. The same lint
//! carries the entropy entries (#511: randomness drawn without a generator's
//! name, see `crate::entropy`) and the transaction entries (`crate::db`), so
//! these sanctioned sites are the only switch-offs for all of them.
//!
//! The `Default` impls of reqwest's client types cannot be named by that lint
//! (#569). They are closed by type instead: clients are held only as
//! `crate::egress::HttpClient` (no `Default`, built only by the sanctioned
//! builders), and `disallowed-types` refuses the raw reqwest types by name
//! outside `src/egress_http.rs`. Its switches are scanned the same way, and
//! that file, which neither lint constrains, is held to an exact list of its
//! contents ([`CLIENT_MODULE`]).
//!
//! Attributes are parsed with `syn` and lint paths compared token by token,
//! so comments inside an attribute and raw identifiers (`r#style`) read the
//! same as the plain form. Attributes written inside macro bodies are parsed
//! from the macro's tokens.
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::path::{Component, Path, PathBuf};
use syn::{Meta, Token, ext::IdentExt, punctuated::Punctuated, visit::Visit};

/// Lint levels that can lower `-D clippy::disallowed_methods`. `warn` is
/// still an error under CI's `-D warnings`, but it is refused anyway: it
/// would let the lint pass a plain `cargo clippy`.
const LEVELS: [&str; 3] = ["allow", "expect", "warn"];

/// The two lints, their old names, the two groups that contain them, and
/// `warnings`.
const SWITCHES: [&str; 7] = [
    "clippy::disallowed_methods",
    "clippy::disallowed_method",
    "clippy::disallowed_types",
    "clippy::disallowed_type",
    "clippy::style",
    "clippy::all",
    "warnings",
];

#[derive(Clone, Debug, PartialEq)]
enum Finding {
    /// A lint level naming one of [`SWITCHES`], with the enclosing
    /// `cfg_attr` predicates and the `reason`, if any.
    Level {
        level: String,
        lint: String,
        inner: bool,
        cfg: Vec<String>,
        reason: Option<String>,
        /// What the attribute is attached to: `crate`, `fn <path>`,
        /// `let in <fn path>`, or `other in <path>` (never sanctioned).
        on: String,
    },
    /// Code the scan would not read: a `#[path]` leaving `src/` or naming a
    /// file that is not `*.rs`, or `include!`.
    Escape(String),
    /// A lint-level or `cfg_attr` attribute that does not parse.
    Unparsed(String),
    /// An attribute or macro call the scan cannot read: one built from a
    /// macro metavariable (`#[$m]`, `#[path = $p]`, `$i!`), or one in a
    /// macro body that `syn` cannot parse. The scan fails closed on these.
    Unreadable(String),
}

fn path_name(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.unraw().to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join("::")
}

fn meta_text(meta: &Meta) -> String {
    let text = match meta {
        Meta::Path(path) => path_name(path),
        Meta::List(list) => format!("{}({})", path_name(&list.path), list.tokens),
        Meta::NameValue(value) => format!("{}=..", path_name(&value.path)),
    };
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Lexically joins `value` to `dir`; `None` if it climbs above the root.
fn join(dir: &Path, value: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in dir.join(value).components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    Some(out)
}

struct Scan<'a> {
    src: &'a Path,
    /// The directory of the scanned file. A `#[path]` inside an inline
    /// module resolves below it, so staying in `src/` from here is enough.
    dir: &'a Path,
    findings: Vec<Finding>,
    /// The enclosing modules and functions.
    path: Vec<String>,
    /// Attributes already read with the item they are attached to.
    read: std::collections::HashSet<*const syn::Attribute>,
}

impl Scan<'_> {
    fn here(&self) -> String {
        self.path.join("::")
    }

    /// Reads an item's own attributes, labelled with what they sit on.
    fn own(&mut self, attributes: &[syn::Attribute], on: &str) {
        for attribute in attributes {
            self.read.insert(attribute);
            let inner = matches!(attribute.style, syn::AttrStyle::Inner(_));
            self.meta(&attribute.meta, inner, &[], on);
        }
    }

    fn meta(&mut self, meta: &Meta, inner: bool, cfg: &[String], on: &str) {
        let args = |list: &syn::MetaList| {
            list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        };
        match meta {
            Meta::List(list) if path_name(&list.path) == "cfg_attr" => {
                let Ok(args) = args(list) else {
                    return self.findings.push(Finding::Unparsed(meta_text(meta)));
                };
                let mut args = args.into_iter();
                let mut cfg = cfg.to_vec();
                cfg.push(args.next().as_ref().map(meta_text).unwrap_or_default());
                for attribute in args {
                    self.meta(&attribute, inner, &cfg, on);
                }
            }
            Meta::List(list) if LEVELS.contains(&path_name(&list.path).as_str()) => {
                let Ok(args) = args(list) else {
                    return self.findings.push(Finding::Unparsed(meta_text(meta)));
                };
                let reason = args.iter().find_map(|arg| match arg {
                    Meta::NameValue(value) if path_name(&value.path) == "reason" => {
                        match &value.value {
                            syn::Expr::Lit(syn::ExprLit {
                                lit: syn::Lit::Str(text),
                                ..
                            }) => Some(text.value()),
                            _ => Some(String::new()),
                        }
                    }
                    _ => None,
                });
                for arg in &args {
                    let Meta::Path(path) = arg else { continue };
                    let lint = path_name(path);
                    if SWITCHES.contains(&lint.as_str()) {
                        self.findings.push(Finding::Level {
                            level: path_name(&list.path),
                            lint,
                            inner,
                            cfg: cfg.to_vec(),
                            reason: reason.clone(),
                            on: on.to_owned(),
                        });
                    }
                }
            }
            Meta::NameValue(value) if path_name(&value.path) == "path" => {
                let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(text),
                    ..
                }) = &value.value
                else {
                    return self
                        .findings
                        .push(Finding::Escape("#[path = <not a string literal>]".into()));
                };
                let target = text.value();
                // The walk reads `*.rs` only, so a module in a file of any
                // other name would be compiled but never scanned.
                let inside = !Path::new(&target).is_absolute()
                    && target.ends_with(".rs")
                    && join(self.dir, &target).is_some_and(|path| path.starts_with(self.src));
                if !inside {
                    self.findings
                        .push(Finding::Escape(format!("#[path = {target:?}]")));
                }
            }
            _ => {}
        }
    }

    /// Attributes, `include` and metavariable macro calls inside a macro's
    /// tokens (`macro_rules!` bodies, macro calls), which `syn` leaves
    /// unparsed. What cannot be read is a finding.
    fn tokens(&mut self, tokens: TokenStream) {
        let punct = |token: Option<&TokenTree>, c: char| matches!(token, Some(TokenTree::Punct(p)) if p.as_char() == c);
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        for (at, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Punct(p) if p.as_char() == '#' => {
                    let inner = punct(tokens.get(at + 1), '!');
                    let Some(TokenTree::Group(group)) = tokens.get(at + 1 + usize::from(inner))
                    else {
                        continue;
                    };
                    if group.delimiter() != Delimiter::Bracket {
                        continue;
                    }
                    let text = format!("#[{}]", group.stream());
                    if has_metavariable(group.stream()) {
                        self.findings.push(Finding::Unreadable(text));
                    } else if let Ok(meta) = syn::parse2::<Meta>(group.stream()) {
                        let on = format!("other in {}", self.here());
                        self.meta(&meta, inner, &[], &on);
                    } else {
                        self.findings.push(Finding::Unreadable(text));
                    }
                }
                TokenTree::Punct(p) if p.as_char() == '$' => {
                    if matches!(tokens.get(at + 1), Some(TokenTree::Ident(_)))
                        && punct(tokens.get(at + 2), '!')
                    {
                        self.findings
                            .push(Finding::Unreadable(format!("${}!", tokens[at + 1])));
                    }
                }
                TokenTree::Ident(ident) if ident.unraw() == "include" => {
                    self.findings
                        .push(Finding::Escape("include in a macro".into()));
                }
                TokenTree::Group(group) => self.tokens(group.stream()),
                _ => {}
            }
        }
    }
}

fn has_metavariable(tokens: TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Punct(p) => p.as_char() == '$',
        TokenTree::Group(group) => has_metavariable(group.stream()),
        _ => false,
    })
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_file(&mut self, file: &'ast syn::File) {
        self.own(&file.attrs, "crate");
        syn::visit::visit_file(self, file);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.path.push(item.sig.ident.unraw().to_string());
        let on = format!("fn {}", self.here());
        self.own(&item.attrs, &on);
        syn::visit::visit_item_fn(self, item);
        self.path.pop();
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.path.push(item.sig.ident.unraw().to_string());
        syn::visit::visit_impl_item_fn(self, item);
        self.path.pop();
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.path.push(item.ident.unraw().to_string());
        syn::visit::visit_item_mod(self, item);
        self.path.pop();
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        let on = format!("let in {}", self.here());
        self.own(&local.attrs, &on);
        syn::visit::visit_local(self, local);
    }

    /// Any attribute not read with its item: never a sanctioned site.
    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        if !self.read.contains(&(attribute as *const _)) {
            let inner = matches!(attribute.style, syn::AttrStyle::Inner(_));
            let on = format!("other in {}", self.here());
            self.meta(&attribute.meta, inner, &[], &on);
        }
        syn::visit::visit_attribute(self, attribute);
    }

    fn visit_use_name(&mut self, name: &'ast syn::UseName) {
        if name.ident.unraw() == "include" {
            self.findings.push(Finding::Escape("use of include".into()));
        }
    }

    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        if rename.ident.unraw() == "include" {
            self.findings.push(Finding::Escape("use of include".into()));
        }
        syn::visit::visit_use_rename(self, rename);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident.unraw() == "include")
        {
            self.findings.push(Finding::Escape("include!".into()));
        }
        self.tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }
}

/// Every lint switch and every escape from `src/` in one file.
fn scan(src: &Path, file: &Path, source: &str) -> Vec<Finding> {
    let ast = syn::parse_file(source).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let mut scan = Scan {
        src,
        dir: file.parent().unwrap(),
        findings: Vec::new(),
        path: Vec::new(),
        read: std::collections::HashSet::new(),
    };
    scan.visit_file(&ast);
    scan.findings
}

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

fn level(level: &str, inner: bool, cfg: &[&str], reason: Option<&str>, on: &str) -> Finding {
    level_of(METHODS, level, inner, cfg, reason, on)
}

const METHODS: &str = "clippy::disallowed_methods";
const TYPES: &str = "clippy::disallowed_types";

fn level_of(
    lint: &str,
    level: &str,
    inner: bool,
    cfg: &[&str],
    reason: Option<&str>,
    on: &str,
) -> Finding {
    Finding::Level {
        level: level.into(),
        lint: lint.into(),
        inner,
        cfg: cfg.iter().map(|c| (*c).to_owned()).collect(),
        reason: reason.map(str::to_owned),
        on: on.into(),
    }
}

/// The sanctioned sites of each file under `src/`; nothing for any other.
fn sanctioned(name: &str) -> Vec<Finding> {
    let guarded = |on: &str| {
        level(
            "expect",
            false,
            &[],
            Some("the egress-guarded constructor (#509)"),
            on,
        )
    };
    match name {
        // The one module that holds or builds a raw reqwest client (#569):
        // the whole file may name the types; only the three constructors
        // may call the disallowed methods.
        "egress_http.rs" => vec![
            level_of(
                TYPES,
                "expect",
                true,
                &[],
                Some("the egress client newtype is the one place a reqwest client is held (#569)"),
                "crate",
            ),
            guarded("fn client_builder"),
            guarded("fn pinned_client_builder"),
            guarded("fn add_root_certificates"),
        ],
        "db.rs" => vec![level("allow", false, &[], None, "let in start")],
        // tungstenite fills the upstream handshake's Sec-WebSocket-Key from
        // its own generator: a protocol nonce, not a gateway secret (#511).
        "providers/openai/realtime.rs" => vec![level(
            "expect",
            false,
            &[],
            Some("tungstenite's Sec-WebSocket-Key is a handshake nonce, not a secret (#511)"),
            "fn handshake_request",
        )],
        "lib.rs" => vec![
            level("allow", true, &["test"], None, "crate"),
            level_of(TYPES, "allow", true, &["test"], None, "crate"),
        ],
        _ => Vec::new(),
    }
}

/// Every file under `src/` (production and unit tests alike) may switch the
/// lint off only at these sites, and pulls in no code from outside `src/`.
/// Unit tests need no site of their own: `lib.rs` allows the lint for the
/// test build, and the non-test build, which Clippy also checks, contains
/// all production code. Integration tests under `tests/` are their own
/// crates and opt out per file.
#[test]
fn only_the_sanctioned_sites_turn_off_disallowed_methods() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut all = Vec::new();
    files(&src, &mut all);
    assert!(
        all.len() > 200,
        "found {} files; did the scan break?",
        all.len()
    );
    for path in all {
        let name = path
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let found = scan(&src, &path, &std::fs::read_to_string(&path).unwrap());
        assert_eq!(
            found,
            sanctioned(&name),
            "{name}: a lint switch for clippy::disallowed_methods or clippy::disallowed_types \
             outside its sanctioned sites, or code from outside src/. Production code builds \
             HTTP clients with crate::egress::client_builder (#509), holds them as \
             crate::egress::HttpClient (#569) and draws secrets with crate::entropy (#511)."
        );
    }
}

/// Synthetic sources: each form that switches the lint off, or pulls in
/// code from outside `src/`, is found; look-alikes are not.
#[test]
fn the_scan_sees_every_spelling_of_a_lint_switch() {
    let src = Path::new("/repo/apps/gateway/src");
    let file = src.join("module/file.rs");
    let finds = |source: &str| !scan(src, &file, source).is_empty();
    for source in [
        "#[allow(clippy::disallowed_methods)] fn f() {}",
        "#[allow(clippy::/**/disallowed_methods)] fn f() {}",
        "#[allow(/* x */ clippy::style)] fn f() {}",
        "#[allow(clippy:: // c\n disallowed_methods)] fn f() {}",
        "#[allow(clippy :: disallowed_methods)] fn f() {}",
        "#[allow(clippy::r#disallowed_methods)] fn f() {}",
        "#[allow(clippy::r#style)] fn f() {}",
        "#[allow(r#clippy::all)] fn f() {}",
        "#[allow(clippy::DISALLOWED_METHODS)] fn f() {}",
        "#[allow(clippy::disallowed_method)] fn f() {}",
        "#[allow(clippy::disallowed_types)] struct S { c: reqwest::Client }",
        "#[expect(clippy::disallowed_types, reason = \"x\")] fn f() {}",
        "#[allow(clippy::disallowed_type)] fn f() {}",
        "#![cfg_attr(not(test), allow(clippy::disallowed_types))]",
        "mod m { #![allow(clippy::disallowed_types)] }",
        "impl S { #[allow(clippy::r#disallowed_types)] fn f() {} }",
        "#[allow(dead_code, warnings)] fn f() {}",
        "#[expect(clippy::all, reason = \"x\")] fn f() {}",
        "#[warn(clippy::disallowed_methods)] fn f() {}",
        "#![allow(clippy::disallowed_methods)]",
        "#![cfg_attr(not(test), allow(clippy::style))]",
        "#[cfg_attr(a, cfg_attr(b, cfg_attr(c, expect(clippy::disallowed_methods))))] fn f() {}",
        "#[cfg_attr(a, inline, allow(clippy::disallowed_methods))] fn f() {}",
        "fn f() { #[allow(clippy::disallowed_methods)] let x = 1; }",
        "impl S { #[allow(clippy::disallowed_methods)] fn f() {} }",
        "mod m { #![allow(clippy::disallowed_methods)] }",
        "macro_rules! m { () => { #[allow(clippy::disallowed_methods)] fn f() {} } }",
        "macro_rules! m { () => { #![allow(clippy::style)] } }",
        "m! { #[allow(warnings)] fn f() {} }",
        "#[path = \"../../x.rs\"] mod m;",
        "#[path = \"../../../x.rs\"] mod m;",
        "#[path = \"/tmp/x.rs\"] mod m;",
        "#[cfg_attr(test, path = \"../../x.rs\")] mod m;",
        "#[path = \"probe_hidden.inc\"] mod hidden;",
        "#[path = \"hidden\"] mod hidden;",
        "#[path = \"hidden.RS\"] mod hidden;",
        "#[path = \"hidden.rs.txt\"] mod hidden;",
        "#[cfg_attr(test, path = \"hidden.inc\")] mod hidden;",
        "mod m { #[path = \"hidden.inc\"] mod hidden; }",
        "macro_rules! m { () => { #[path = \"hidden.inc\"] mod hidden; } }",
        "mod m { #[path = \"../../x.rs\"] mod n; }",
        "include!(\"x.rs\");",
        "fn f() { include!(\"../../x.rs\") }",
        "fn f() { std::include!(\"x.rs\") }",
        "macro_rules! m { () => { include!(\"x.rs\"); } }",
    ] {
        assert!(finds(source), "missed: {source}");
    }
    for source in [
        "#[allow(clippy::needless_return)] fn f() {}",
        "#[allow(dead_code, clippy::disallowed_methods_x)] fn f() {}",
        "#[deny(clippy::disallowed_methods)] fn f() {}",
        "#[deny(clippy::disallowed_types)] fn f() {}",
        "#[allow(clippy::disallowed_types_x)] fn f() {}",
        "#[forbid(warnings)] fn f() {}",
        "#[path = \"x.rs\"] mod m;",
        "#[path = \"../sibling/x.rs\"] mod m;",
        "#[doc = include_str!(\"../../../docs/x.md\")] fn f() {}",
        "fn f() { let _ = \"#[allow(clippy::disallowed_methods)]\"; }",
        "// #[allow(clippy::disallowed_methods)]\nfn f() {}",
    ] {
        assert!(!finds(source), "false positive: {source}");
    }
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
        "reqwest::ClientBuilder::windows_named_pipe",
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
        clippy_entry(&config, path);
    }
}

/// The `Default` routes (#569): Clippy cannot name `<Client as
/// Default>::default`, so the raw types are refused by name and the two
/// `RequestBuilder` methods that hand out or accept a raw client are refused.
#[test]
fn clippy_disallows_the_raw_client_types() {
    let config =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("clippy.toml")).unwrap();
    let config = uncommented(&config);
    let (methods, types) = config
        .split_once("disallowed-types = [")
        .expect("clippy.toml must have a disallowed-types list (#569)");
    for path in ["reqwest::Client", "reqwest::ClientBuilder"] {
        let line = clippy_entry(types, path);
        assert!(
            !line.contains("allow-invalid"),
            "clippy.toml: the type {path} must resolve"
        );
    }
    for path in [
        "reqwest::RequestBuilder::build_split",
        "reqwest::RequestBuilder::from_parts",
    ] {
        let line = clippy_entry(methods, path);
        assert!(
            !line.contains("allow-invalid"),
            "clippy.toml: the method {path} must resolve"
        );
    }
}

/// The `clippy.toml` line that disallows `path`; its reason must point at
/// `crate::egress`.
fn clippy_entry<'a>(list: &'a str, path: &str) -> &'a str {
    let entry = format!("{{ path = \"{path}\", reason = \"");
    let line = list
        .lines()
        .find(|line| line.trim_start().starts_with(&entry))
        .unwrap_or_else(|| panic!("clippy.toml must disallow {path} (#509, #569)"));
    assert!(
        line.contains("crate::egress::"),
        "clippy.toml: the reason for {path} must name its crate::egress alternative"
    );
    line
}

/// Everything `src/egress_http.rs` may contain (#569), one entry per
/// top-level item and per method of an inherent impl, as its tokens joined
/// by single spaces without doc comments (see [`text`]). That file is the
/// one place Clippy lets a raw reqwest client be named, and its three
/// constructors may call the disallowed builder methods, so nothing in it is
/// trusted by shape: an item, method, signature, attribute or body that is
/// not listed here fails the test. Change this list only with the file, in a
/// reviewed change.
const CLIENT_MODULE: &[&str] = &[
    "# ! [ expect ( clippy : : disallowed_types , reason = \"the egress client newtype is the one place a reqwest client is held (#569)\" ) ]",
    "use std : : { sync : : Arc , time : : Duration } ;",
    "use super : : Resolver ;",
    "# [ derive ( Clone , Debug ) ] pub struct HttpClient ( reqwest : : Client ) ;",
    "# [ derive ( Debug ) ] # [ must_use ] pub ( crate ) struct HttpClientBuilder ( reqwest : : ClientBuilder ) ;",
    "trait AmbiguousIfDefault < A > { fn some_item ( ) { } }",
    "impl < T : ? Sized > AmbiguousIfDefault < ( ) > for T { }",
    "impl < T : Default > AmbiguousIfDefault < u8 > for T { }",
    "const _ : fn ( ) = | | { let _ = < HttpClient as AmbiguousIfDefault < _ > > : : some_item ; let _ = < HttpClientBuilder as AmbiguousIfDefault < _ > > : : some_item ; } ;",
    "impl HttpClient { }",
    "HttpClient :: pub fn get < U : reqwest : : IntoUrl > ( & self , url : U ) - > reqwest : : RequestBuilder { self . 0 . get ( url ) }",
    "HttpClient :: pub fn post < U : reqwest : : IntoUrl > ( & self , url : U ) - > reqwest : : RequestBuilder { self . 0 . post ( url ) }",
    "HttpClient :: pub fn delete < U : reqwest : : IntoUrl > ( & self , url : U ) - > reqwest : : RequestBuilder { self . 0 . delete ( url ) }",
    "HttpClient :: pub async fn execute ( & self , request : reqwest : : Request ) - > reqwest : : Result < reqwest : : Response > { self . 0 . execute ( request ) . await }",
    "HttpClient :: pub fn request < U : reqwest : : IntoUrl > ( & self , method : reqwest : : Method , url : U , ) - > reqwest : : RequestBuilder { self . 0 . request ( method , url ) }",
    "HttpClient :: # [ cfg ( test ) ] pub ( crate ) fn from_test_client ( client : reqwest : : Client ) - > Self { Self ( client ) }",
    "impl HttpClientBuilder { }",
    "HttpClientBuilder :: pub fn timeout ( self , timeout : Duration ) - > Self { Self ( self . 0 . timeout ( timeout ) ) }",
    "HttpClientBuilder :: pub fn connect_timeout ( self , timeout : Duration ) - > Self { Self ( self . 0 . connect_timeout ( timeout ) ) }",
    "HttpClientBuilder :: pub fn read_timeout ( self , timeout : Duration ) - > Self { Self ( self . 0 . read_timeout ( timeout ) ) }",
    "HttpClientBuilder :: pub fn pool_max_idle_per_host ( self , max : usize ) - > Self { Self ( self . 0 . pool_max_idle_per_host ( max ) ) }",
    "HttpClientBuilder :: pub fn pool_idle_timeout ( self , timeout : Duration ) - > Self { Self ( self . 0 . pool_idle_timeout ( timeout ) ) }",
    "HttpClientBuilder :: pub fn https_only ( self , enabled : bool ) - > Self { Self ( self . 0 . https_only ( enabled ) ) }",
    "HttpClientBuilder :: pub fn build ( self ) - > reqwest : : Result < HttpClient > { self . 0 . build ( ) . map ( HttpClient ) }",
    "# [ expect ( clippy : : disallowed_methods , reason = \"the egress-guarded constructor (#509)\" ) ] pub ( crate ) fn client_builder ( resolver : Arc < Resolver > ) - > HttpClientBuilder { HttpClientBuilder ( reqwest : : Client : : builder ( ) . no_proxy ( ) . dns_resolver ( resolver ) . redirect ( reqwest : : redirect : : Policy : : none ( ) ) . retry ( reqwest : : retry : : never ( ) ) , ) }",
    "# [ expect ( clippy : : disallowed_methods , reason = \"the egress-guarded constructor (#509)\" ) ] pub ( crate ) fn pinned_client_builder ( approved : & crate : : providers : : local : : endpoints : : ApprovedDestinations , ) - > HttpClientBuilder { HttpClientBuilder ( reqwest : : Client : : builder ( ) . no_proxy ( ) . redirect ( reqwest : : redirect : : Policy : : none ( ) ) . retry ( reqwest : : retry : : never ( ) ) . resolve_to_addrs ( approved . host ( ) , approved . addresses ( ) ) , ) }",
    "# [ expect ( clippy : : disallowed_methods , reason = \"the egress-guarded constructor (#509)\" ) ] pub ( crate ) fn add_root_certificates ( builder : HttpClientBuilder , certificates : impl IntoIterator < Item = reqwest : : Certificate > , ) - > HttpClientBuilder { let mut builder = builder . 0 ; for certificate in certificates { builder = builder . add_root_certificate ( certificate ) ; } HttpClientBuilder ( builder ) }",
];

fn client_module_source() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/egress_http.rs"))
        .unwrap()
}

/// `tokens` joined by single spaces, without doc comments: the same text
/// for any layout or comment, and for any `proc_macro2` version.
fn text(tokens: TokenStream) -> String {
    fn push(tokens: TokenStream, out: &mut Vec<String>) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        let punct = |at: usize, c: char| matches!(tokens.get(at), Some(TokenTree::Punct(p)) if p.as_char() == c);
        let mut at = 0;
        while at < tokens.len() {
            if punct(at, '#') {
                let group = at + 1 + usize::from(punct(at + 1, '!'));
                if let Some(TokenTree::Group(attribute)) = tokens.get(group)
                    && attribute.delimiter() == Delimiter::Bracket
                    && matches!(
                        attribute.stream().into_iter().next(),
                        Some(TokenTree::Ident(name)) if name.unraw() == "doc"
                    )
                {
                    at = group + 1;
                    continue;
                }
            }
            match &tokens[at] {
                TokenTree::Group(group) => {
                    let (open, close) = match group.delimiter() {
                        Delimiter::Parenthesis => ("(", ")"),
                        Delimiter::Brace => ("{", "}"),
                        Delimiter::Bracket => ("[", "]"),
                        Delimiter::None => ("", ""),
                    };
                    out.push(open.into());
                    push(group.stream(), out);
                    out.push(close.into());
                }
                other => out.push(other.to_string()),
            }
            at += 1;
        }
    }
    let mut out = Vec::new();
    push(tokens, &mut out);
    out.retain(|token| !token.is_empty());
    out.join(" ")
}

/// The entries of a source file in [`CLIENT_MODULE`]'s form: inner
/// attributes, then each item; an inherent impl is its header followed by
/// one `Type :: member` entry per member.
fn module_entries(file: &syn::File) -> Vec<String> {
    use quote::ToTokens;
    let mut entries: Vec<String> = file
        .attrs
        .iter()
        .map(|attribute| text(attribute.to_token_stream()))
        .filter(|entry| !entry.is_empty())
        .collect();
    for item in &file.items {
        match item {
            syn::Item::Impl(item) if item.trait_.is_none() => {
                let mut header = item.clone();
                header.items.clear();
                entries.push(text(header.to_token_stream()));
                let owner = text(item.self_ty.to_token_stream());
                for member in &item.items {
                    entries.push(format!("{owner} :: {}", text(member.to_token_stream())));
                }
            }
            other => entries.push(text(other.to_token_stream())),
        }
    }
    entries
}

/// Why `source` is not the reviewed client module: entries that are not on
/// [`CLIENT_MODULE`] or are missing from the file, and, whatever the list
/// says, anything in the file that could make or release an unguarded
/// client: `Default`/`default` in any spelling (only the proof's one
/// `T: Default` bound may name it), `take`, `transmute`, `unsafe`, an item
/// nested in a body, and any macro call.
fn client_module_problems(source: &str) -> Vec<String> {
    let (Ok(file), Ok(tokens)) = (syn::parse_file(source), source.parse::<TokenStream>()) else {
        return vec!["the file does not parse".into()];
    };
    let entries = module_entries(&file);
    let mut problems: Vec<String> = entries
        .iter()
        .filter(|entry| !CLIENT_MODULE.contains(&entry.as_str()))
        .map(|entry| format!("not on the list: {entry}"))
        .collect();
    problems.extend(
        CLIENT_MODULE
            .iter()
            .filter(|listed| !entries.iter().any(|entry| entry == *listed))
            .map(|listed| format!("missing from the file: {listed}")),
    );
    if problems.is_empty() && entries != CLIENT_MODULE {
        problems.push("the listed entries are reordered or repeated".into());
    }

    fn idents(tokens: TokenStream, out: &mut Vec<String>) {
        for token in tokens {
            match token {
                TokenTree::Ident(ident) => out.push(ident.unraw().to_string()),
                TokenTree::Group(group) => idents(group.stream(), out),
                _ => {}
            }
        }
    }
    let mut names = Vec::new();
    idents(tokens, &mut names);
    let bounds = names.iter().filter(|name| *name == "Default").count();
    if bounds > 1 {
        problems.push(format!(
            "`Default` is named {bounds} times: only the proof's `T: Default` bound may name it"
        ));
    }
    for name in names {
        let lower = name.to_lowercase();
        let refused =
            (lower.contains("default") && name != "Default" && name != "AmbiguousIfDefault")
                || ["take", "transmute", "transmute_copy", "unsafe"].contains(&name.as_str());
        if refused {
            problems.push(format!("`{name}` in the client module"));
        }
    }

    struct Bodies(Vec<String>);
    impl<'ast> Visit<'ast> for Bodies {
        fn visit_block(&mut self, block: &'ast syn::Block) {
            if block
                .stmts
                .iter()
                .any(|statement| matches!(statement, syn::Stmt::Item(_)))
            {
                self.0.push("an item nested in a body".into());
            }
            syn::visit::visit_block(self, block);
        }
        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            self.0
                .push(format!("a macro call: {}!", path_name(&mac.path)));
        }
    }
    let mut bodies = Bodies(Vec::new());
    bodies.visit_file(&file);
    problems.extend(bodies.0);
    problems
}

/// `source` with `from` (which must occur exactly once) replaced by `to`.
fn edited(source: &str, from: &str, to: &str) -> String {
    assert_eq!(source.matches(from).count(), 1, "not found once: {from}");
    source.replacen(from, to, 1)
}

/// Changes to the client module that would hand out, accept or build an
/// unguarded client, or weaken the no-`Default` proof, each with the reason
/// the check must give. The first group are the review's planted variants
/// (#569): none of them writes a raw type outside the file, and none was
/// refused by Clippy or by a list of bad shapes.
fn planted(source: &str) -> Vec<(String, &'static str)> {
    let add = |code: &str| format!("{source}\n{code}");
    vec![
        (
            add("pub type Raw = reqwest::Client; \
                 impl HttpClient { pub fn into_raw(self) -> Raw { self.0 } }"),
            "not on the list: pub type Raw = reqwest :: Client ;",
        ),
        (
            add("pub type Raw = reqwest::Client; \
                 impl HttpClient { pub fn into_raw(self) -> Raw { self.0 } }"),
            "not on the list: HttpClient :: pub fn into_raw (self) -> Raw",
        ),
        (
            add("use std::ops::Deref as D; impl D for HttpClient { \
                 type Target = reqwest::Client; fn deref(&self) -> &reqwest::Client { &self.0 } }"),
            "not on the list: impl D for HttpClient",
        ),
        (
            add("use std::ops::Deref as D;"),
            "not on the list: use std :: ops :: Deref as D ;",
        ),
        (
            add("impl HttpClient { pub fn ambient() -> Self { Self(Default::default()) } }"),
            "`Default` is named 2 times",
        ),
        (
            add("impl HttpClient { pub fn ambient() -> Self { Self(Default::default()) } }"),
            "not on the list: HttpClient :: pub fn ambient () -> Self",
        ),
        (
            add("impl HttpClientBuilder { pub fn ambient() -> Self { Self(Default::default()) } }"),
            "`default` in the client module",
        ),
        (
            add("pub struct Sibling(reqwest::Client); impl Sibling { \
                 pub fn new() -> Self { Self(reqwest::Client::default()) } }"),
            "not on the list: pub struct Sibling (reqwest :: Client) ;",
        ),
        (
            add("pub struct Sibling(reqwest::Client); impl Sibling { \
                 pub fn new() -> Self { Self(reqwest::Client::default()) } }"),
            "`default` in the client module",
        ),
        (
            add("pub static RAW: std::sync::LazyLock<reqwest::Client> = \
                 std::sync::LazyLock::new(reqwest::Client::new);"),
            "not on the list: pub static RAW",
        ),
        (
            add(
                "pub async fn fetch(url: &str) -> reqwest::Result<reqwest::Response> { \
                 reqwest::Client::default().get(url).send().await }",
            ),
            "not on the list: pub async fn fetch",
        ),
        (
            add("impl TryFrom<u8> for HttpClient { type Error = (); \
                 fn try_from(_: u8) -> Result<Self, ()> { Ok(Self(Default::default())) } }"),
            "not on the list: impl TryFrom < u8 > for HttpClient",
        ),
        (
            edited(
                source,
                "Self(self.0.timeout(timeout))",
                "impl std::ops::Deref for HttpClient { type Target = reqwest::Client; \
                 fn deref(&self) -> &reqwest::Client { &self.0 } } Self(self.0.timeout(timeout))",
            ),
            "an item nested in a body",
        ),
        (
            edited(
                source,
                "pub fn build(self)",
                "pub fn min_tls_version(self, version: reqwest::tls::Version) -> Self { \
                 Self(self.0.min_tls_version(version)) } pub fn build(self)",
            ),
            "not on the list: HttpClientBuilder :: pub fn min_tls_version",
        ),
        (
            edited(
                source,
                "pub fn build(self)",
                "pub fn tls_sni(self, sni: bool) -> Self { Self(self.0.tls_sni(sni)) } \
                 pub fn build(self)",
            ),
            "not on the list: HttpClientBuilder :: pub fn tls_sni",
        ),
        (
            edited(
                source,
                "pub fn build(self)",
                "pub fn default_headers(self, headers: reqwest::header::HeaderMap) -> Self { \
                 Self(self.0.default_headers(headers)) } pub fn build(self)",
            ),
            "not on the list: HttpClientBuilder :: pub fn default_headers",
        ),
        // Further routes to a default or raw client.
        (
            add("impl HttpClient { pub fn fresh(&mut self) -> Self { \
                 Self(std::mem::take(&mut self.0)) } }"),
            "`take` in the client module",
        ),
        (
            add("impl HttpClient { pub fn maybe(c: Option<Self>) -> Self { \
                 Self(c.map(|c| c.0).unwrap_or_default()) } }"),
            "`unwrap_or_default` in the client module",
        ),
        (
            add("pub fn forge() -> HttpClient { unsafe { std::mem::transmute(0usize) } }"),
            "`transmute` in the client module",
        ),
        (
            add("pub fn forge() -> HttpClient { unsafe { std::mem::zeroed() } }"),
            "`unsafe` in the client module",
        ),
        (
            add("impl Default for HttpClient { fn default() -> Self { todo!() } }"),
            "not on the list: impl Default for HttpClient",
        ),
        (
            add("impl From<reqwest::Client> for HttpClient { \
                 fn from(client: reqwest::Client) -> Self { Self(client) } }"),
            "not on the list: impl From < reqwest :: Client > for HttpClient",
        ),
        (
            add("mod inner { pub fn raw() -> reqwest::Client { reqwest::Client::new() } }"),
            "not on the list: mod inner",
        ),
        (add("m! { fn f() {} }"), "a macro call: m!"),
        (
            add("extern \"C\" { fn raw_client() -> usize; }"),
            "not on the list: extern",
        ),
        (
            edited(
                source,
                "pub struct HttpClient(reqwest::Client);",
                "pub struct HttpClient(pub reqwest::Client);",
            ),
            "missing from the file: # [derive (Clone , Debug)] pub struct HttpClient (reqwest :: Client) ;",
        ),
        (
            edited(
                source,
                "#[derive(Debug)]\n#[must_use]",
                "#[derive(Debug, Default)]\n#[must_use]",
            ),
            "missing from the file: # [derive (Debug)] # [must_use] pub (crate) struct HttpClientBuilder",
        ),
        (
            edited(source, "    #[cfg(test)]\n", ""),
            "missing from the file: HttpClient :: # [cfg (test)] pub (crate) fn from_test_client",
        ),
        (
            edited(
                source,
                "pub fn get<U: reqwest::IntoUrl>(&self, url: U)",
                "pub fn get<U: reqwest::IntoUrl>(self, url: U)",
            ),
            "missing from the file: HttpClient :: pub fn get",
        ),
        (
            edited(source, "self.0.post(url)", "self.0.clone().post(url)"),
            "missing from the file: HttpClient :: pub fn post",
        ),
        // The three constructors sit under `expect(disallowed_methods)`.
        (
            edited(
                source,
                ".dns_resolver(resolver)",
                ".dns_resolver(resolver).danger_accept_invalid_certs(true)",
            ),
            "missing from the file: # [expect (clippy :: disallowed_methods , reason = \"the egress-guarded constructor (#509)\")] pub (crate) fn client_builder",
        ),
        (
            edited(source, ".dns_resolver(resolver)", ""),
            "changed: pub(crate) fn client_builder",
        ),
        (
            edited(
                source,
                ".resolve_to_addrs(approved.host(), approved.addresses())",
                ".resolve_to_addrs(approved.host(), approved.addresses()).proxy(reqwest::Proxy::all(\"http://127.0.0.1:1\").unwrap())",
            ),
            "changed: pub(crate) fn pinned_client_builder",
        ),
        (
            edited(
                source,
                "builder = builder.add_root_certificate(certificate);",
                "builder = builder.add_root_certificate(certificate).danger_accept_invalid_hostnames(true);",
            ),
            "changed: pub(crate) fn add_root_certificates",
        ),
        (
            edited(
                source,
                "clippy::disallowed_types,",
                "clippy::disallowed_types, clippy::disallowed_methods,",
            ),
            "missing from the file: # ! [ expect ( clippy : : disallowed_types , reason",
        ),
        // The proof that neither type implements `Default`.
        (
            edited(
                source,
                "    let _ = <HttpClient as AmbiguousIfDefault<_>>::some_item;\n",
                "",
            ),
            "missing from the file: const _ : fn ()",
        ),
        (
            edited(
                source,
                "    let _ = <HttpClientBuilder as AmbiguousIfDefault<_>>::some_item;\n",
                "",
            ),
            "missing from the file: const _ : fn ()",
        ),
        (
            edited(
                source,
                "    let _ = <HttpClient as AmbiguousIfDefault<_>>::some_item;\n",
                "    // let _ = <HttpClient as AmbiguousIfDefault<_>>::some_item;\n",
            ),
            "missing from the file: const _ : fn ()",
        ),
        (
            edited(
                source,
                "impl<T: ?Sized> AmbiguousIfDefault<()> for T {}\n",
                "",
            ),
            "missing from the file: impl < T : ? Sized > AmbiguousIfDefault < () > for T { }",
        ),
        (
            edited(
                source,
                "impl<T: Default> AmbiguousIfDefault<u8> for T {}\n",
                "",
            ),
            "missing from the file: impl < T : Default > AmbiguousIfDefault < u8 > for T { }",
        ),
        (
            edited(
                source,
                "impl<T: Default> AmbiguousIfDefault<u8>",
                "impl<T: Copy> AmbiguousIfDefault<u8>",
            ),
            "missing from the file: impl < T : Default > AmbiguousIfDefault < u8 > for T { }",
        ),
        (
            edited(
                source,
                "impl<T: Default> AmbiguousIfDefault<u8>",
                "impl<T: Default + Copy> AmbiguousIfDefault<u8>",
            ),
            "missing from the file: impl < T : Default > AmbiguousIfDefault < u8 > for T { }",
        ),
        (
            edited(
                source,
                "trait AmbiguousIfDefault<A> {\n    fn some_item() {}",
                "trait AmbiguousIfDefault<A> {\n    fn some_item() where Self: Sized {}",
            ),
            "missing from the file: trait AmbiguousIfDefault < A > { fn some_item () { } }",
        ),
    ]
}

/// A planted change's reason with its code in [`text`]'s form, so the
/// reasons in [`planted`] can be written as ordinary Rust.
fn rendered(reason: &str) -> String {
    for prefix in [
        "not on the list: HttpClientBuilder :: ",
        "not on the list: HttpClient :: ",
        "missing from the file: HttpClient :: ",
        "not on the list: ",
        "missing from the file: ",
        "changed: ",
    ] {
        if let Some(code) = reason.strip_prefix(prefix)
            && let Ok(tokens) = code.parse::<TokenStream>()
        {
            return format!("{}{}", prefix.replace("changed: ", ""), text(tokens));
        }
    }
    reason.to_owned()
}

/// `src/egress_http.rs` is exactly the reviewed list (#569). It is the one
/// file where a raw reqwest client may be named and where the disallowed
/// builder methods may be called, so the Clippy rule says nothing about it.
#[test]
fn the_client_module_is_exactly_the_reviewed_list() {
    let problems = client_module_problems(&client_module_source());
    assert!(
        problems.is_empty(),
        "apps/gateway/src/egress_http.rs differs from the reviewed list (CLIENT_MODULE in \
         egress_source_scan_tests.rs). It is the only code that may hold or build a raw \
         reqwest client, so nothing in it is accepted by shape: change the list deliberately, \
         in a reviewed change (#569).\n{}",
        problems.join("\n")
    );
}

/// Every planted change to the client module is refused, for its own reason.
#[test]
fn the_client_module_list_refuses_every_planted_change() {
    let source = client_module_source();
    for (changed, reason) in planted(&source) {
        let reason = rendered(reason);
        let problems = client_module_problems(&changed);
        assert!(
            problems.iter().any(|problem| problem.contains(&reason)),
            "expected {reason:?}, found {problems:#?}"
        );
    }
    // Layout and comments are not part of the list.
    let relaid = source.replace("\n\n", "\n// a comment\n\n/// More documentation.\n");
    assert_eq!(client_module_problems(&relaid), Vec::<String>::new());
}

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Text without TOML comments (a `#` inside a string is rare enough in
/// these files to be read as a comment, which only makes the check stricter
/// by not hiding anything).
fn uncommented(text: &str) -> String {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lints configured in Cargo (`[lints]`, `[workspace.lints]`, in any TOML
/// spelling) could switch the lint off for the whole crate.
#[test]
fn no_cargo_lints_table_overrides_the_rule() {
    for manifest in ["Cargo.toml", "apps/gateway/Cargo.toml"] {
        let text = std::fs::read_to_string(repository().join(manifest)).unwrap();
        assert!(
            !uncommented(&text).contains("lints"),
            "{manifest}: a Cargo lints table could switch off clippy::disallowed_methods (#509)"
        );
    }
}

/// Walks the repository, skipping build output, dependencies and git.
fn repository_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            if !(name == ".git"
                || name == "node_modules"
                || name == ".local"
                || name.starts_with("target"))
            {
                repository_files(&path, out);
            }
        } else {
            out.push(path);
        }
    }
}

/// CI and the local gates: workflows, package scripts and `scripts/`.
fn gate_file(name: &str) -> bool {
    name.starts_with(".github/") || name.starts_with("scripts/") || name.ends_with("package.json")
}

/// What in a CI or gate file could replace or lower the Clippy rule. Any
/// case, with `-` and `_` alike, as Cargo and rustc read them.
fn gate_problems(text: &str) -> Vec<String> {
    let text = text.to_lowercase().replace('-', "_");
    let mut problems: Vec<String> = [
        "clippy_conf_dir",
        "rustflags",
        "cap_lints",
        "disallowed_method",
        "disallowed_type",
        "clippy::style",
        "clippy::all",
    ]
    .into_iter()
    .filter(|needle| text.contains(needle))
    .map(str::to_owned)
    .collect();
    // `-A warnings`, `-Awarnings`, `--allow=warnings`, `-W`/`--warn` alike.
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || "='\",[]".contains(c))
        .filter(|word| !word.is_empty())
        .collect();
    for (at, word) in words.iter().enumerate() {
        let argument = match *word {
            "_a" | "__allow" | "_w" | "__warn" => words.get(at + 1).copied(),
            _ if (word.starts_with("_a") || word.starts_with("_w")) && !word.starts_with("__") => {
                Some(&word[2..])
            }
            _ => None,
        };
        if argument == Some("warnings") {
            problems.push(format!("{word} warnings"));
        }
    }
    problems
}

/// Another `clippy.toml`/`.clippy.toml` (or `CLIPPY_CONF_DIR`) would replace
/// `apps/gateway/clippy.toml`; a `.cargo/config` could add `-A` flags or
/// `--cap-lints`; and CI must not set them either.
#[test]
fn no_other_configuration_replaces_or_lowers_the_rule() {
    let root = repository();
    let mut all = Vec::new();
    repository_files(&root, &mut all);
    assert!(
        all.len() > 500,
        "found {} files; did the walk break?",
        all.len()
    );
    let mut workflows = 0;
    for path in all {
        let name = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let file = path.file_name().unwrap().to_string_lossy();
        if file == "clippy.toml" || file == ".clippy.toml" {
            assert_eq!(
                name, "apps/gateway/clippy.toml",
                "a second Clippy configuration (#509)"
            );
        }
        assert!(
            !(name.contains(".cargo/") && (file == "config" || file == "config.toml")),
            "{name}: a Cargo configuration could lower the Clippy rule (#509)"
        );
        if gate_file(&name) {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let problems = gate_problems(&text);
            assert!(
                problems.is_empty(),
                "{name}: {problems:?} could lower the Clippy rule (#509)"
            );
            workflows += usize::from(name.starts_with(".github/"));
        }
    }
    assert!(
        workflows > 3,
        "found {workflows} CI files; did the walk break?"
    );
}

/// Attributes and macro calls the scan cannot read are findings, not skipped
/// (#509): it fails closed.
#[test]
fn the_scan_fails_closed_on_what_it_cannot_read() {
    let src = Path::new("/repo/apps/gateway/src");
    let file = src.join("module/file.rs");
    for source in [
        "macro_rules! m { ($m:meta) => { #[$m] fn f() {} } } m!(allow(clippy::disallowed_methods));",
        "macro_rules! m { ($l:ident) => { #[$l(clippy::disallowed_methods)] fn f() {} } } m!(allow);",
        "macro_rules! m { ($($t:tt)*) => { #[$($t)*] fn f() {} } }",
        "macro_rules! m { ($p:literal) => { #[path = $p] mod n; } }",
        "macro_rules! m { () => { #![$x] } }",
        "#[path = concat!(\"../\", \"x.rs\")] mod m;",
        "#[cfg_attr(test, path = env!(\"X\"))] mod m;",
        "use std::include as inc; inc!(\"../../x.rs\");",
        "use core::include;",
        "use std::{include as i};",
        "pub use std::include;",
        "macro_rules! m { ($i:ident) => { $i!(\"../../x.rs\"); } } m!(include);",
        "m! { use std::include as inc; }",
    ] {
        assert!(!scan(src, &file, source).is_empty(), "missed: {source}");
    }
    for source in [
        "macro_rules! m { ($e:expr) => { #[inline] fn f() -> u8 { $e } } }",
        "macro_rules! m { () => { #[cfg(test)] fn f() {} } }",
        "#[path = \"x.rs\"] mod m;",
    ] {
        assert!(
            scan(src, &file, source).is_empty(),
            "false positive: {source}"
        );
    }
}

/// A sanctioned attribute counts only on its own item: the named function
/// (or, in `db.rs`, the statement in the named function, and in `lib.rs`, the
/// crate root). The same attribute on a module, an impl or another function
/// is refused (#509).
#[test]
fn sanctioned_attributes_count_only_on_their_own_items() {
    let src = Path::new("/repo/apps/gateway/src");
    let expect =
        "#[expect(clippy::disallowed_methods, reason = \"the egress-guarded constructor (#509)\")]";
    let types = "#![expect(clippy::disallowed_types, reason = \"the egress client newtype is the one place a reqwest client is held (#569)\")]";
    let egress = |rogue: &str| {
        format!(
            "{types} {expect} fn client_builder() {{}} {expect} fn pinned_client_builder() {{}} {rogue}"
        )
    };
    let finds = |name: &str, source: &str| scan(src, &src.join(name), source) == sanctioned(name);
    assert!(finds(
        "egress_http.rs",
        &egress(&format!("{expect} fn add_root_certificates() {{}}"))
    ));
    for rogue in [
        format!("{expect} mod wrapped {{ fn rogue() {{}} }}"),
        format!("{expect} fn rogue() {{}}"),
        format!("{expect} impl S {{ fn add_root_certificates() {{}} }}"),
        format!("mod m {{ {expect} fn add_root_certificates() {{}} }}"),
        format!("fn add_root_certificates() {{ {expect} let x = 1; }}"),
        format!("{expect} fn add_root_certificates() {{}} mod m {{ {types} }}"),
        format!(
            "{expect} fn add_root_certificates() {{}} \
             #[expect(clippy::disallowed_types, reason = \"x\")] struct S;"
        ),
    ] {
        assert!(
            !finds("egress_http.rs", &egress(&rogue)),
            "accepted: {rogue}"
        );
    }
    let allow = "#[allow(clippy::disallowed_methods)]";
    assert!(finds(
        "db.rs",
        &format!("async fn start() {{ async move {{ {allow} let begin = 1; }}; }}")
    ));
    for rogue in [
        format!("{allow} mod m {{ fn rogue() {{}} }}"),
        format!("{allow} fn start() {{}}"),
        format!("fn other() {{ {allow} let begin = 1; }}"),
    ] {
        assert!(!finds("db.rs", &rogue), "accepted: {rogue}");
    }
    assert!(finds(
        "lib.rs",
        "#![cfg_attr(test, allow(clippy::disallowed_methods, clippy::disallowed_types))]"
    ));
    for rogue in [
        "mod m { #![cfg_attr(test, allow(clippy::disallowed_methods, clippy::disallowed_types))] }",
        "#![cfg_attr(test, allow(clippy::disallowed_methods))] #![allow(clippy::disallowed_types)]",
        // The file-level switch for the raw types belongs to egress_http.rs only.
        "#![cfg_attr(test, allow(clippy::disallowed_methods, clippy::disallowed_types))] \
         #![expect(clippy::disallowed_types, reason = \"the egress client newtype is the one place a reqwest client is held (#569)\")]",
    ] {
        assert!(!finds("lib.rs", rogue), "accepted: {rogue}");
    }
    // The same switch at the top of any other file is refused.
    assert!(!finds("providers/openai.rs", types));
}

/// CI and gate files: any case, `-` and `_` alike, and the command-line
/// forms (#509).
#[test]
fn gate_files_cannot_lower_the_rule_in_any_spelling() {
    for text in [
        "env:\n  rustflags: -Awarnings",
        "CARGO_ENCODED_RUSTFLAGS=x",
        "clippy-conf-dir: /tmp",
        "cargo clippy -- -A clippy::disallowed-methods",
        "cargo clippy -- -A clippy::disallowed-types",
        "cargo clippy -- -A clippy::style",
        "cargo clippy -- -Awarnings",
        "cargo clippy -- -A warnings",
        "cargo clippy -- --allow=warnings",
        "cargo clippy -- -W clippy::all",
        "cargo clippy -- --warn warnings",
        "cargo clippy -- --cap-lints warn",
        "cargo clippy --config 'build.rustflags=[\"-A\", \"warnings\"]'",
        "\"lint\": \"cargo clippy -- -D warnings -A clippy::DISALLOWED_METHODS\"",
    ] {
        assert!(!gate_problems(text).is_empty(), "missed: {text}");
    }
    for text in [
        "cargo clippy --workspace --all-targets --all-features -- -D warnings",
        "ls -a warnings.txt",
        "tar -A archive",
    ] {
        assert!(gate_problems(text).is_empty(), "false positive: {text}");
    }
}
