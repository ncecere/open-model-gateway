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

/// The lint, its old name, the two groups that contain it, and `warnings`.
const SWITCHES: [&str; 5] = [
    "clippy::disallowed_methods",
    "clippy::disallowed_method",
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
    /// Code from outside `src/`: a `#[path]` leaving it, or `include!`.
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
                let inside = !Path::new(&target).is_absolute()
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
    Finding::Level {
        level: level.into(),
        lint: SWITCHES[0].into(),
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
        "egress.rs" => vec![
            guarded("fn client_builder"),
            guarded("fn pinned_client_builder"),
            guarded("fn add_root_certificates"),
        ],
        "db.rs" => vec![level("allow", false, &[], None, "let in start")],
        "lib.rs" => vec![level("allow", true, &["test"], None, "crate")],
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
            "{name}: a lint switch for clippy::disallowed_methods outside its sanctioned \
             sites, or code from outside src/. Production code builds HTTP clients with \
             crate::egress::client_builder (#509) and draws secrets with crate::entropy (#511)."
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
    let egress = |rogue: &str| {
        format!(
            "{expect} fn client_builder() {{}} {expect} fn pinned_client_builder() {{}} {rogue}"
        )
    };
    let finds = |name: &str, source: &str| scan(src, &src.join(name), source) == sanctioned(name);
    assert!(finds(
        "egress.rs",
        &egress(&format!("{expect} fn add_root_certificates() {{}}"))
    ));
    for rogue in [
        format!("{expect} mod wrapped {{ fn rogue() {{}} }}"),
        format!("{expect} fn rogue() {{}}"),
        format!("{expect} impl S {{ fn add_root_certificates() {{}} }}"),
        format!("mod m {{ {expect} fn add_root_certificates() {{}} }}"),
        format!("fn add_root_certificates() {{ {expect} let x = 1; }}"),
    ] {
        assert!(!finds("egress.rs", &egress(&rogue)), "accepted: {rogue}");
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
        "#![cfg_attr(test, allow(clippy::disallowed_methods))]"
    ));
    assert!(!finds(
        "lib.rs",
        "mod m { #![cfg_attr(test, allow(clippy::disallowed_methods))] }"
    ));
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
