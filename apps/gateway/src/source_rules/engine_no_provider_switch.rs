//! No provider switches in the engine (AGENTS.md invariant, #591): "add
//! providers through `ProviderAdapter` and `ProviderRegistry`; do not add
//! provider switches to the engine". A provider is named by a string
//! (`Deployment.provider`, an adapter's `ProviderAdapter::id`, the fixed
//! telemetry enum), so a switch is a comparison against that string or a
//! reference to one adapter's type or module. In the production build the
//! scanned files must contain none of:
//!
//! * a string literal equal (any case) to a provider id, including inside
//!   macro bodies and `matches!`;
//! * a reference to an adapter type (`AnthropicAdapter`, `LocalAdapter`, ...)
//!   or to a provider module (`providers::anthropic`, `providers::local`, ...);
//! * a variant of the telemetry `Provider` enum (`Provider::Anthropic`).
//!
//! The provider ids, adapter types, provider modules and enum variants are
//! read from `providers/` and `telemetry.rs` (each must still include a
//! pinned floor, so a refactor that hides them fails instead of passing
//! empty). Scanned: the engine, `inference/` (no exceptions allowed), and the
//! files that admit and route its requests ([`ADJACENT`]) with the exact
//! exceptions in [`ALLOWED`].
//!
//! Cannot see: a provider named by a string built at run time or read from
//! configuration; a comparison of two values neither of which is a literal
//! (`a.provider == b.provider` is allowed, it is not a switch); a different
//! spelling of the id ("open-ai"); a provider type behind an alias made
//! outside the scanned files; behaviour keyed on a capability that is really
//! a proxy for one provider.
use std::collections::BTreeSet;

use proc_macro2::TokenTree;
use syn::{ext::IdentExt, visit::Visit};

use super::tree::{Scope, Source, flat};

/// The engine proper: nothing is allowed here.
const ENGINE: &[&str] = &["inference/"];

/// Files that admit and route the engine's requests: scanned too, with
/// [`ALLOWED`] for what exists today.
const ADJACENT: &[&str] = &[
    "governance.rs",
    "governance/",
    "routing.rs",
    "routing/",
    "jobs/",
];

/// (file, item, what) found in [`ADJACENT`] today. The engine, `inference/`,
/// has none. These are outside it, in admission and the batch scheduler:
///
/// * Anthropic native cache controls are priced and held by comparing the
///   provider name in admission (`governance.rs`, `governance/batch.rs`) and
///   in batch line planning (`jobs/plan/scan.rs`): provider-specific
///   behaviour that belongs behind `ProviderAdapter`. Real switches; not
///   fixed in #591 (no behaviour change). Tracked as a follow-up issue.
/// * `jobs/schedule.rs` `PRIORITY_PROFILES` lists the local profiles whose
///   servers expose scheduling metrics (`vllm`, `openai_compatible`): also a
///   provider-kind switch. Same follow-up.
/// * `providers::local::endpoints::ApprovedEndpoints` in `jobs/mod.rs` and
///   `jobs/schedule.rs` is the shared, server-controlled approval of local
///   destinations (AGENTS.md: local HTTP endpoints). It is a type about
///   destinations, not a branch on the provider; listed because the scan
///   cannot tell it from an adapter module.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "governance.rs",
        "admit_unobserved",
        "provider id literal \"anthropic\"",
    ),
    (
        "governance/batch.rs",
        "admit_line_unobserved",
        "provider id literal \"anthropic\"",
    ),
    (
        "jobs/plan/scan.rs",
        "impl Jobs::price_line",
        "provider id literal \"anthropic\"",
    ),
    (
        "jobs/schedule.rs",
        "<file>",
        "provider id literal \"openai_compatible\"",
    ),
    ("jobs/schedule.rs", "<file>", "provider id literal \"vllm\""),
    (
        "jobs/schedule.rs",
        "<file>",
        "provider module providers::local",
    ),
    ("jobs/mod.rs", "<file>", "provider module providers::local"),
    (
        "jobs/mod.rs",
        "impl Jobs::with_approvals",
        "provider module providers::local",
    ),
];

const FLOOR_IDS: &[&str] = &[
    "openai",
    "anthropic",
    "bedrock",
    "openrouter",
    "openai_compatible",
    "vllm",
    "sglang",
    "ollama",
];
const FLOOR_MODULES: &[&str] = &["anthropic", "bedrock", "local", "openai", "openrouter"];
const FLOOR_ADAPTERS: &[&str] = &[
    "AnthropicAdapter",
    "BedrockAdapter",
    "LocalAdapter",
    "OpenAiAdapter",
    "OpenRouterAdapter",
];

#[derive(Default, Debug)]
pub(super) struct Names {
    ids: BTreeSet<String>,
    modules: BTreeSet<String>,
    adapters: BTreeSet<String>,
    variants: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Hit {
    file: String,
    item: String,
    what: String,
}

impl Hit {
    fn text(&self) -> String {
        format!("{} :: {} :: {}", self.file, self.item, self.what)
    }
}

fn strings_of(tokens: proc_macro2::TokenStream) -> Vec<String> {
    flat(tokens)
        .into_iter()
        .filter_map(|t| match t {
            TokenTree::Literal(l) => match syn::parse_str::<syn::Lit>(&l.to_string()) {
                Ok(syn::Lit::Str(s)) => Some(s.value()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[derive(Default)]
struct IdReader {
    in_id: bool,
    ids: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for IdReader {
    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        let was = self.in_id;
        self.in_id |= f.sig.ident == "id";
        syn::visit::visit_impl_item_fn(self, f);
        self.in_id = was;
    }
    fn visit_lit_str(&mut self, lit: &'ast syn::LitStr) {
        if self.in_id {
            self.ids.insert(lit.value());
        }
    }
}

/// Reads the provider names out of `providers/` and `telemetry.rs`.
pub(super) fn names(sources: &[Source]) -> Names {
    let mut names = Names::default();
    for source in sources {
        if let Some(rest) = source.path.strip_prefix("providers/") {
            let module = rest.split('/').next().unwrap_or("").trim_end_matches(".rs");
            let mut reader = IdReader::default();
            reader.visit_file(&source.file);
            names.ids.extend(reader.ids);
            for item in &source.file.items {
                adapters(item, module, &mut names);
            }
        }
        if source.path == "telemetry.rs" {
            for item in &source.file.items {
                if let syn::Item::Macro(m) = item
                    && m.mac.path.is_ident("fixed_enum")
                {
                    let tokens = flat(m.mac.tokens.clone());
                    if !matches!(tokens.first(), Some(TokenTree::Ident(i)) if i == "Provider") {
                        continue;
                    }
                    names.ids.extend(strings_of(m.mac.tokens.clone()));
                    for pair in tokens.windows(3) {
                        if let [
                            TokenTree::Ident(variant),
                            TokenTree::Punct(a),
                            TokenTree::Punct(b),
                        ] = pair
                            && a.as_char() == '='
                            && b.as_char() == '>'
                        {
                            names.variants.insert(variant.unraw().to_string());
                        }
                    }
                }
            }
        }
    }
    names
}

fn adapters(item: &syn::Item, module: &str, names: &mut Names) {
    match item {
        syn::Item::Impl(imp) => {
            if let Some((trait_path, _)) = &imp.trait_
                && trait_path
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == "ProviderAdapter")
            {
                names.adapters.insert(super::tree::type_name(&imp.self_ty));
                names.modules.insert(module.to_owned());
            }
        }
        syn::Item::Mod(m) => {
            for inner in m.content.iter().flat_map(|(_, items)| items) {
                adapters(inner, module, names);
            }
        }
        _ => {}
    }
}

/// Problems with the discovery itself.
pub(super) fn name_problems(names: &Names) -> Vec<String> {
    let mut out = Vec::new();
    let mut need = |what: &str, have: &BTreeSet<String>, floor: &[&str]| {
        for name in floor {
            if !have.contains(*name) {
                out.push(format!(
                    "discovery lost the {what} {name:?}: the scan would pass empty"
                ));
            }
        }
    };
    need("provider id", &names.ids, FLOOR_IDS);
    need("provider module", &names.modules, FLOOR_MODULES);
    need("adapter type", &names.adapters, FLOOR_ADAPTERS);
    if names.variants.len() < FLOOR_IDS.len() {
        out.push(format!(
            "discovery found only {:?} telemetry variants",
            names.variants
        ));
    }
    out
}

struct Finder<'a> {
    file: String,
    names: &'a Names,
    scope: Scope,
    hits: BTreeSet<Hit>,
}

impl Finder<'_> {
    fn hit(&mut self, what: String) {
        self.hits.insert(Hit {
            file: self.file.clone(),
            item: self.scope.path(),
            what,
        });
    }

    fn literal(&mut self, text: &str) {
        if self
            .names
            .ids
            .iter()
            .any(|id| id.eq_ignore_ascii_case(text))
        {
            self.hit(format!("provider id literal {text:?}"));
        }
    }

    fn segments(&mut self, segments: &[String]) {
        for pair in segments.windows(2) {
            if pair[0] == "providers" && self.names.modules.contains(&pair[1]) {
                self.hit(format!("provider module providers::{}", pair[1]));
            }
            if pair[0] == "Provider" && self.names.variants.contains(&pair[1]) {
                self.hit(format!("telemetry variant Provider::{}", pair[1]));
            }
        }
        for segment in segments {
            if self.names.adapters.contains(segment) {
                self.hit(format!("adapter type {segment}"));
            }
        }
    }

    fn tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.unraw().to_string());
                self.tree(&p.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(n) => {
                prefix.push(n.ident.unraw().to_string());
                self.segments(prefix);
                prefix.pop();
            }
            syn::UseTree::Rename(r) => {
                prefix.push(r.ident.unraw().to_string());
                self.segments(prefix);
                prefix.pop();
            }
            syn::UseTree::Glob(_) => self.segments(prefix),
            syn::UseTree::Group(g) => {
                for inner in &g.items {
                    self.tree(inner, prefix);
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    track_scope!();

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments: Vec<String> = path
            .segments
            .iter()
            .map(|s| s.ident.unraw().to_string())
            .collect();
        self.segments(&segments);
        syn::visit::visit_path(self, path);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.tree(&item.tree, &mut Vec::new());
    }

    fn visit_lit_str(&mut self, lit: &'ast syn::LitStr) {
        self.literal(&lit.value());
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        syn::visit::visit_macro(self, mac);
        let tokens = flat(mac.tokens.clone());
        for text in strings_of(mac.tokens.clone()) {
            self.literal(&text);
        }
        // Idents joined by `::` inside a macro body.
        let mut run: Vec<String> = Vec::new();
        for (at, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Ident(i) => run.push(i.unraw().to_string()),
                TokenTree::Punct(p) if p.as_char() == ':' => {}
                _ => {
                    self.segments(&run);
                    run.clear();
                }
            }
            if at + 1 == tokens.len() {
                self.segments(&run);
            }
        }
    }
}

fn applies(path: &str, prefixes: &[&str]) -> bool {
    prefixes
        .iter()
        .any(|p| path == *p || (p.ends_with('/') && path.starts_with(p)))
}

pub(super) fn scan(source: &Source, names: &Names) -> BTreeSet<Hit> {
    let mut finder = Finder {
        file: source.path.clone(),
        names,
        scope: Scope::default(),
        hits: BTreeSet::new(),
    };
    finder.visit_file(&source.file);
    finder.hits
}

/// Hits in the engine, and hits in the adjacent files that are not allowed
/// (or allowed entries that no longer match).
pub(super) fn problems(
    sources: &[Source],
    names: &Names,
    allowed: &[(&str, &str, &str)],
) -> Vec<String> {
    let mut engine = BTreeSet::new();
    let mut adjacent = BTreeSet::new();
    for source in sources {
        if applies(&source.path, ENGINE) {
            engine.extend(scan(source, names));
        } else if applies(&source.path, ADJACENT) {
            adjacent.extend(scan(source, names));
        }
    }
    let allowed: BTreeSet<Hit> = allowed
        .iter()
        .map(|(file, item, what)| Hit {
            file: (*file).to_owned(),
            item: (*item).to_owned(),
            what: (*what).to_owned(),
        })
        .collect();
    let mut out: Vec<String> = engine
        .iter()
        .map(|h| format!("provider switch in the engine: {}", h.text()))
        .collect();
    out.extend(
        adjacent
            .difference(&allowed)
            .map(|h| format!("provider switch in admission/routing: {}", h.text())),
    );
    out.extend(
        allowed
            .difference(&adjacent)
            .map(|h| format!("stale allowlist entry: {}", h.text())),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::super::tree::{parse_fixture, production};
    use super::*;

    fn real() -> Vec<Source> {
        production("lib.rs")
    }

    #[test]
    fn the_engine_has_no_provider_switch() {
        let sources = real();
        let names = names(&sources);
        let mut problems = name_problems(&names);
        problems.extend(self::problems(&sources, &names, ALLOWED));
        assert!(
            problems.is_empty(),
            "providers are reached only through ProviderAdapter and ProviderRegistry (AGENTS.md, \
             #591): put provider-specific behaviour in the adapter, not behind a comparison of \
             the provider name:\n{}",
            problems.join("\n")
        );
    }

    #[test]
    fn the_engine_files_are_scanned() {
        let sources = real();
        for path in [
            "inference/mod.rs",
            "inference/attempt.rs",
            "inference/workload.rs",
        ] {
            assert!(
                sources.iter().any(|s| s.path == path),
                "{path} is not scanned"
            );
        }
        assert!(
            sources
                .iter()
                .all(|s| !s.path.starts_with("inference/tests"))
        );
    }

    fn fixture_names() -> Names {
        let providers = parse_fixture(
            "providers/anthropic.rs",
            r#"impl ProviderAdapter for AnthropicAdapter { fn id(&self) -> &'static str { "anthropic" } }"#,
        );
        let others = [
            ("providers/openai.rs", "OpenAiAdapter", "openai"),
            ("providers/bedrock.rs", "BedrockAdapter", "bedrock"),
            ("providers/openrouter.rs", "OpenRouterAdapter", "openrouter"),
        ]
        .map(|(path, ty, id)| {
            parse_fixture(
                path,
                &format!("impl ProviderAdapter for {ty} {{ fn id(&self) -> &'static str {{ \"{id}\" }} }}"),
            )
        });
        let local = parse_fixture(
            "providers/local/mod.rs",
            r#"impl ProviderAdapter for LocalAdapter { fn id(&self) -> &'static str { match 1 { _ => "vllm" } } }"#,
        );
        let telemetry = parse_fixture(
            "telemetry.rs",
            r#"fixed_enum!(Provider { OpenAi => "openai", Anthropic => "anthropic", Compatible => "openai_compatible", Ollama => "ollama", Sglang => "sglang", Bedrock => "bedrock", OpenRouter => "openrouter", Vllm => "vllm" });"#,
        );
        let mut all = vec![providers, local, telemetry];
        all.extend(others);
        names(&all)
    }

    fn engine(code: &str) -> Vec<String> {
        let names = fixture_names();
        assert!(
            name_problems(&names).is_empty(),
            "{:?}",
            name_problems(&names)
        );
        problems(&[parse_fixture("inference/attempt.rs", code)], &names, &[])
    }

    #[test]
    fn discovery_reads_ids_adapters_modules_and_variants() {
        let names = fixture_names();
        assert!(names.ids.contains("openai_compatible") && names.ids.contains("vllm"));
        assert!(names.adapters.contains("LocalAdapter"));
        assert!(names.modules.contains("local"));
        assert!(names.variants.contains("Sglang"));
    }

    #[test]
    fn comparing_the_provider_name_is_refused() {
        let found = engine(
            r#"fn f(d: &Deployment) -> bool { d.provider == "anthropic" || "OpenAI" != d.provider }"#,
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains("provider id literal \"anthropic\"")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains("provider id literal \"OpenAI\"")),
            "{found:?}"
        );
    }

    #[test]
    fn matching_on_the_provider_name_is_refused() {
        let found = engine(
            r#"fn f(p: &str) { match p { "bedrock" => {}, _ => {} } let _ = matches!(p, "ollama" | "vllm"); }"#,
        );
        for id in ["bedrock", "ollama", "vllm"] {
            assert!(found.iter().any(|p| p.contains(id)), "{id} in {found:?}");
        }
    }

    #[test]
    fn naming_an_adapter_type_or_provider_module_is_refused() {
        let found = engine(
            "use crate::providers::anthropic::AnthropicAdapter;\n\
             use crate::providers::{local::LocalAdapter, openai};\n\
             fn f() { let _ = crate::providers::bedrock::new(); let _: Option<OpenRouterAdapter> = None; }",
        );
        for what in [
            "adapter type AnthropicAdapter",
            "provider module providers::anthropic",
            "adapter type LocalAdapter",
            "provider module providers::local",
            "provider module providers::openai",
            "provider module providers::bedrock",
            "adapter type OpenRouterAdapter",
        ] {
            assert!(
                found.iter().any(|p| p.contains(what)),
                "{what} in {found:?}"
            );
        }
    }

    #[test]
    fn a_telemetry_provider_variant_is_refused_but_the_table_lookup_is_not() {
        let found = engine("fn f() { let _ = telemetry::Provider::Anthropic; }");
        assert!(
            found.iter().any(|p| p.contains("Provider::Anthropic")),
            "{found:?}"
        );
        let found = engine(
            "fn f(x: &str) -> Option<telemetry::Provider> { telemetry::Provider::ALL.iter().copied().find(|p| p.as_str() == x) }",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn registry_use_and_unrelated_strings_pass() {
        let found = engine(
            r#"use crate::providers::{ProviderAdapter, ProviderRegistry};
               fn f(r: &ProviderRegistry, d: &Deployment) { let _ = r.get(&d.provider); let _ = "openai-beta"; let _ = "provider"; }"#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn engine_test_code_is_not_scanned() {
        let found = engine(r#"#[cfg(test)] mod tests { fn t() { let _ = "anthropic"; } }"#);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn adjacent_files_are_held_to_the_exact_allowlist() {
        let names = fixture_names();
        let source = parse_fixture(
            "governance.rs",
            r#"fn admit(d: &D) { if d.provider == "anthropic" {} }"#,
        );
        let sources = [source];
        let found = problems(&sources, &names, &[]);
        assert_eq!(found.len(), 1, "{found:?}");
        let allowed = [(
            "governance.rs",
            "admit",
            "provider id literal \"anthropic\"",
        )];
        assert!(problems(&sources, &names, &allowed).is_empty());
        let stale = [(
            "governance.rs",
            "other",
            "provider id literal \"anthropic\"",
        )];
        assert_eq!(problems(&sources, &names, &stale).len(), 2);
    }

    #[test]
    fn engine_hits_cannot_be_allowlisted() {
        let names = fixture_names();
        let source = parse_fixture(
            "inference/mod.rs",
            r#"fn f(p: &str) { if p == "openai" {} }"#,
        );
        let allowed = [("inference/mod.rs", "f", "provider id literal \"openai\"")];
        let found = problems(&[source], &names, &allowed);
        assert!(
            found
                .iter()
                .any(|p| p.starts_with("provider switch in the engine")),
            "{found:?}"
        );
    }

    #[test]
    fn a_lost_discovery_fails_instead_of_passing_empty() {
        let found = name_problems(&Names::default());
        assert!(found.len() >= FLOOR_IDS.len() + FLOOR_MODULES.len());
    }
}
