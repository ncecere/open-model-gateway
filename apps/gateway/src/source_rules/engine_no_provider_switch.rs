//! No provider switches in the engine (AGENTS.md invariant, #591): "add
//! providers through `ProviderAdapter` and `ProviderRegistry`; do not add
//! provider switches to the engine". A provider is named by a string
//! (`Deployment.provider`, an adapter's `ProviderAdapter::id`) or by a
//! provider-kind enum (`telemetry::Provider`, `providers::messages::
//! ProviderClass`, `egress_upstream::UpstreamClient`, the local `Profile`), so
//! a switch is a comparison against one of those names or a reference to one
//! adapter's type or module. In the production build the scanned files must
//! contain none of:
//!
//! * a string or byte-string literal equal (any case) to a provider id,
//!   including inside macro bodies and `matches!`;
//! * a string literal that starts with a model-family or provider spelling
//!   ([`MODEL_PREFIXES`], pinned), or that is the argument of `starts_with`,
//!   `contains`, `ends_with`, `strip_prefix`, `strip_suffix`, `find` or
//!   `eq_ignore_ascii_case` and contains one of [`MODEL_SPELLINGS`];
//! * a reference to an adapter type (`AnthropicAdapter`, `LocalAdapter`, ...)
//!   or to a provider module (`providers::anthropic`, `providers::local`,
//!   `providers::messages`, ...);
//! * a variant of any provider-kind enum (`Provider::Anthropic`,
//!   `ProviderClass::Bedrock`, `UpstreamClient::Local`), also through
//!   `use ... as P` and by a glob import of the enum.
//!
//! In `inference/` there is a fifth rule: the only items it may take from
//! `providers` are `ProviderAdapter` and `ProviderRegistry`.
//!
//! The provider ids, adapter types, provider modules and provider-kind enums
//! are read from the tree: ids from `ProviderAdapter::id` bodies and the
//! `fixed_enum!` tables, adapters and modules from `impl ProviderAdapter`, and
//! an enum is provider-kind when at least three of its variants are provider
//! spellings (`openai`, `anthropic`, `bedrock`, `openrouter`, `local`,
//! `compatible`, `ollama`, `vllm`, `sglang`). Each has a pinned floor, so a
//! refactor that hides them fails instead of passing empty. Scanned: the
//! engine, `inference/` (no exceptions allowed), and the files that admit and
//! route its requests ([`ADJACENT`]) against the exact, counted exceptions in
//! [`ALLOWED`].
//!
//! Cannot see: a provider named by a string built at run time or read from
//! configuration; ids read through a `const` defined elsewhere (the literal is
//! caught where the const is defined, if that file is scanned, not where it is
//! used); a helper defined outside the scanned files that does the comparison
//! (`deployment.is_anthropic()`); a comparison of two values neither of which
//! is a literal (`a.provider == b.provider` is allowed, it is not a switch); a
//! different spelling of the id ("open-ai"); a model family not in the pinned
//! list; a provider type behind an alias made outside the scanned files;
//! behaviour keyed on a capability that is really a proxy for one provider.
use std::collections::{BTreeMap, BTreeSet};

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

/// The only items `inference/` may take from `providers`.
const ENGINE_PROVIDER_ITEMS: &[&str] = &["ProviderAdapter", "ProviderRegistry"];

/// Spellings a model name starts with (lowercase), pinned.
const MODEL_PREFIXES: &[&str] = &[
    "claude",
    "gpt-",
    "gpt4",
    "o1-",
    "o3-",
    "anthropic/",
    "anthropic.",
    "openai/",
    "openrouter/",
    "bedrock",
    "amazon.",
    "meta.",
    "gemini",
    "llama",
    "mistral",
    "qwen",
    "deepseek",
];

/// Spellings searched for inside the argument of `contains`, `starts_with`
/// and similar (lowercase), pinned.
const MODEL_SPELLINGS: &[&str] = &[
    "claude",
    "gpt-",
    "anthropic",
    "openai",
    "bedrock",
    "openrouter",
    "ollama",
    "vllm",
    "sglang",
    "gemini",
    "llama",
    "mistral",
];

const SEARCH_METHODS: &[&str] = &[
    "starts_with",
    "ends_with",
    "contains",
    "strip_prefix",
    "strip_suffix",
    "find",
    "eq_ignore_ascii_case",
];

/// Variant names (lowercase, `_` removed) that mark a provider-kind enum when
/// at least three of an enum's variants are in this list.
const PROVIDER_VARIANTS: &[&str] = &[
    "openai",
    "anthropic",
    "bedrock",
    "openrouter",
    "local",
    "compatible",
    "openaicompatible",
    "ollama",
    "vllm",
    "sglang",
];

/// Modules of `providers` that are not adapters but name a provider kind.
const EXTRA_MODULES: &[&str] = &["messages"];

/// (file, item, what, count) found in [`ADJACENT`] today. The engine,
/// `inference/`, has none. These are outside it, in admission and the batch
/// scheduler. A second identical hit fails: the count is exact.
///
/// * Anthropic native cache controls are priced and held by comparing the
///   provider name in admission (`governance.rs`, `governance/batch.rs`) and
///   in batch line planning (`jobs/plan/scan.rs`): provider-specific
///   behaviour that belongs behind `ProviderAdapter`. Real switches; not
///   fixed in #591 (no behaviour change). Tracked in #624.
/// * `jobs/schedule.rs` `PRIORITY_PROFILES` lists the local profiles that
///   accept vLLM's `priority` request field (`vllm`, `openai_compatible`):
///   also a provider-kind switch, read elsewhere through the const. Same
///   follow-up.
/// * `providers::local::endpoints::ApprovedEndpoints` in `jobs/mod.rs` and
///   `jobs/schedule.rs` is the shared, server-controlled approval of local
///   destinations (AGENTS.md: local HTTP endpoints). It is a type about
///   destinations, not a branch on the provider; listed because the scan
///   cannot tell it from an adapter module.
/// * `jobs/schedule.rs` `fetch_reading` counts a failed scrape of a local
///   server's metrics against `UpstreamClient::Local`, the egress client
///   class it used: a metric label, not a branch on the provider.
const ALLOWED: &[(&str, &str, &str, usize)] = &[
    (
        "governance.rs",
        "admit_unobserved",
        "provider id literal \"anthropic\"",
        2,
    ),
    (
        "governance/batch.rs",
        "admit_line_unobserved",
        "provider id literal \"anthropic\"",
        1,
    ),
    (
        "jobs/plan/scan.rs",
        "impl Jobs::price_line",
        "provider id literal \"anthropic\"",
        1,
    ),
    (
        "jobs/schedule.rs",
        "PRIORITY_PROFILES",
        "provider id literal \"openai_compatible\"",
        1,
    ),
    (
        "jobs/schedule.rs",
        "PRIORITY_PROFILES",
        "provider id literal \"vllm\"",
        1,
    ),
    (
        "jobs/schedule.rs",
        "<file>",
        "provider module providers::local",
        1,
    ),
    (
        "jobs/schedule.rs",
        "fetch_reading",
        "provider-kind variant UpstreamClient::Local",
        1,
    ),
    (
        "jobs/mod.rs",
        "<file>",
        "provider module providers::local",
        1,
    ),
    (
        "jobs/mod.rs",
        "impl Jobs::with_approvals",
        "provider module providers::local",
        1,
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
const FLOOR_MODULES: &[&str] = &[
    "anthropic",
    "bedrock",
    "local",
    "messages",
    "openai",
    "openrouter",
];
const FLOOR_ADAPTERS: &[&str] = &[
    "AnthropicAdapter",
    "BedrockAdapter",
    "LocalAdapter",
    "OpenAiAdapter",
    "OpenRouterAdapter",
];
const FLOOR_ENUMS: &[&str] = &["Provider", "ProviderClass", "UpstreamClient"];

#[derive(Default, Debug)]
pub(super) struct Names {
    ids: BTreeSet<String>,
    modules: BTreeSet<String>,
    adapters: BTreeSet<String>,
    /// Provider-kind enum name -> its variants.
    enums: BTreeMap<String, BTreeSet<String>>,
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

type Hits = BTreeMap<Hit, usize>;

fn literal_of(literal: &proc_macro2::Literal) -> Option<String> {
    match syn::parse_str::<syn::Lit>(&literal.to_string()) {
        Ok(syn::Lit::Str(s)) => Some(s.value()),
        Ok(syn::Lit::ByteStr(b)) => Some(String::from_utf8_lossy(&b.value()).into_owned()),
        _ => None,
    }
}

fn strings_of(tokens: proc_macro2::TokenStream) -> Vec<String> {
    flat(tokens)
        .into_iter()
        .filter_map(|t| match t {
            TokenTree::Literal(l) => literal_of(&l),
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

fn normalized(variant: &str) -> String {
    variant.to_ascii_lowercase().replace('_', "")
}

/// A provider-kind enum: at least three variants are provider spellings.
fn provider_kind(variants: &BTreeSet<String>) -> bool {
    variants
        .iter()
        .filter(|v| PROVIDER_VARIANTS.contains(&normalized(v).as_str()))
        .count()
        >= 3
}

/// Enums and `fixed_enum!` tables anywhere in the items: name -> variants,
/// with the strings of a `fixed_enum!` table.
fn enums_in(items: &[syn::Item], out: &mut Vec<(String, BTreeSet<String>, Vec<String>)>) {
    for item in items {
        match item {
            syn::Item::Enum(e) => out.push((
                e.ident.unraw().to_string(),
                e.variants
                    .iter()
                    .map(|v| v.ident.unraw().to_string())
                    .collect(),
                Vec::new(),
            )),
            syn::Item::Macro(m) if m.mac.path.is_ident("fixed_enum") => {
                let tokens = flat(m.mac.tokens.clone());
                let Some(TokenTree::Ident(name)) = tokens.first() else {
                    continue;
                };
                let mut variants = BTreeSet::new();
                for pair in tokens.windows(3) {
                    if let [
                        TokenTree::Ident(variant),
                        TokenTree::Punct(a),
                        TokenTree::Punct(b),
                    ] = pair
                        && a.as_char() == '='
                        && b.as_char() == '>'
                    {
                        variants.insert(variant.unraw().to_string());
                    }
                }
                out.push((
                    name.unraw().to_string(),
                    variants,
                    strings_of(m.mac.tokens.clone()),
                ));
            }
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    enums_in(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// Reads the provider names out of the tree.
pub(super) fn names(sources: &[Source]) -> Names {
    let mut names = Names::default();
    names
        .modules
        .extend(EXTRA_MODULES.iter().map(|m| (*m).to_owned()));
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
        let mut found = Vec::new();
        enums_in(&source.file.items, &mut found);
        for (name, variants, strings) in found {
            if provider_kind(&variants) {
                names.ids.extend(strings);
                names.enums.entry(name).or_default().extend(variants);
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
    let enums: BTreeSet<String> = names.enums.keys().cloned().collect();
    need("provider-kind enum", &enums, FLOOR_ENUMS);
    out
}

/// `use a::Provider as P;` anywhere in a file: alias -> original name.
#[derive(Default)]
struct Aliases(BTreeMap<String, String>);

impl<'ast> Visit<'ast> for Aliases {
    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        self.0.insert(
            rename.rename.unraw().to_string(),
            rename.ident.unraw().to_string(),
        );
    }
}

struct Finder<'a> {
    file: String,
    names: &'a Names,
    aliases: BTreeMap<String, String>,
    /// `inference/`: only `ProviderAdapter` and `ProviderRegistry` from `providers`.
    strict: bool,
    scope: Scope,
    hits: Hits,
}

impl Finder<'_> {
    fn hit(&mut self, what: String) {
        *self
            .hits
            .entry(Hit {
                file: self.file.clone(),
                item: self.scope.path(),
                what,
            })
            .or_default() += 1;
    }

    fn literal(&mut self, text: &str) {
        let lower = text.to_ascii_lowercase();
        if self
            .names
            .ids
            .iter()
            .any(|id| id.eq_ignore_ascii_case(text))
        {
            self.hit(format!("provider id literal {text:?}"));
        } else if MODEL_PREFIXES.iter().any(|p| lower.starts_with(p)) {
            self.hit(format!("model-family literal {text:?}"));
        }
    }

    /// The argument of a search method that contains a provider or model spelling.
    fn searched(&mut self, text: &str) {
        let lower = text.to_ascii_lowercase();
        if MODEL_SPELLINGS.iter().any(|s| lower.contains(s)) {
            self.hit(format!("provider or model spelling searched for: {text:?}"));
        }
    }

    fn segments(&mut self, segments: &[String]) {
        let segments: Vec<String> = segments
            .iter()
            .map(|s| self.aliases.get(s).cloned().unwrap_or_else(|| s.clone()))
            .collect();
        for pair in segments.windows(2) {
            if pair[0] == "providers" {
                if self.names.modules.contains(&pair[1]) {
                    self.hit(format!("provider module providers::{}", pair[1]));
                } else if self.strict && !ENGINE_PROVIDER_ITEMS.contains(&pair[1].as_str()) {
                    self.hit(format!(
                        "providers::{} (the engine may take only ProviderAdapter and ProviderRegistry)",
                        pair[1]
                    ));
                }
            }
            if let Some(variants) = self.names.enums.get(&pair[0])
                && variants.contains(&pair[1])
            {
                self.hit(format!("provider-kind variant {}::{}", pair[0], pair[1]));
            }
        }
        for segment in &segments {
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
            syn::UseTree::Glob(_) => {
                self.segments(prefix);
                let last = prefix
                    .last()
                    .map(|l| self.aliases.get(l).cloned().unwrap_or_else(|| l.clone()));
                if let Some(last) = last
                    && self.names.enums.contains_key(&last)
                {
                    self.hit(format!("glob import of the provider-kind enum {last}"));
                }
            }
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

    fn visit_lit_byte_str(&mut self, lit: &'ast syn::LitByteStr) {
        self.literal(&String::from_utf8_lossy(&lit.value()));
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if SEARCH_METHODS.contains(&call.method.unraw().to_string().as_str()) {
            for arg in &call.args {
                if let syn::Expr::Lit(syn::ExprLit { lit, .. }) = arg {
                    match lit {
                        syn::Lit::Str(s) => self.searched(&s.value()),
                        syn::Lit::ByteStr(b) => self.searched(&String::from_utf8_lossy(&b.value())),
                        _ => {}
                    }
                }
            }
        }
        syn::visit::visit_expr_method_call(self, call);
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

pub(super) fn scan(source: &Source, names: &Names) -> Hits {
    let mut aliases = Aliases::default();
    aliases.visit_file(&source.file);
    let mut finder = Finder {
        file: source.path.clone(),
        names,
        aliases: aliases.0,
        strict: applies(&source.path, ENGINE),
        scope: Scope::default(),
        hits: Hits::new(),
    };
    finder.visit_file(&source.file);
    finder.hits
}

fn merge(into: &mut Hits, from: Hits) {
    for (hit, n) in from {
        *into.entry(hit).or_default() += n;
    }
}

/// Hits in the engine, and hits in the adjacent files that differ from the
/// counted allowlist (or allowed entries that no longer match).
pub(super) fn problems(
    sources: &[Source],
    names: &Names,
    allowed: &[(&str, &str, &str, usize)],
) -> Vec<String> {
    let mut engine = Hits::new();
    let mut adjacent = Hits::new();
    for source in sources {
        if applies(&source.path, ENGINE) {
            merge(&mut engine, scan(source, names));
        } else if applies(&source.path, ADJACENT) {
            merge(&mut adjacent, scan(source, names));
        }
    }
    let mut allowed_hits = Hits::new();
    for (file, item, what, count) in allowed {
        merge(
            &mut allowed_hits,
            Hits::from([(
                Hit {
                    file: (*file).to_owned(),
                    item: (*item).to_owned(),
                    what: (*what).to_owned(),
                },
                *count,
            )]),
        );
    }
    let mut out: Vec<String> = engine
        .iter()
        .map(|(h, n)| format!("provider switch in the engine: {} (x{n})", h.text()))
        .collect();
    for (hit, n) in &adjacent {
        match allowed_hits.get(hit) {
            Some(want) if want == n => {}
            Some(want) => out.push(format!(
                "provider switch in admission/routing: {} found x{n}, allowed x{want}",
                hit.text()
            )),
            None => out.push(format!(
                "provider switch in admission/routing: {} (x{n})",
                hit.text()
            )),
        }
    }
    for (hit, want) in &allowed_hits {
        if !adjacent.contains_key(hit) {
            out.push(format!("stale allowlist entry: {} (x{want})", hit.text()));
        }
    }
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
                &format!(
                    "impl ProviderAdapter for {ty} {{ fn id(&self) -> &'static str {{ \"{id}\" }} }}"
                ),
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
        let class = parse_fixture(
            "providers/messages.rs",
            "pub(crate) enum ProviderClass { Anthropic, OpenAi, Bedrock, Local }",
        );
        let upstream = parse_fixture(
            "egress_upstream.rs",
            "pub(crate) enum UpstreamClient { Openai, Openrouter, Anthropic, Bedrock, Local, S3, Export }",
        );
        let unrelated = parse_fixture("other.rs", "pub enum Color { Red, Anthropic, Green }");
        let mut all = vec![providers, local, telemetry, class, upstream, unrelated];
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
    fn discovery_reads_ids_adapters_modules_and_every_provider_kind_enum() {
        let names = fixture_names();
        assert!(names.ids.contains("openai_compatible") && names.ids.contains("vllm"));
        assert!(names.adapters.contains("LocalAdapter"));
        assert!(names.modules.contains("local") && names.modules.contains("messages"));
        let enums: Vec<&String> = names.enums.keys().collect();
        assert_eq!(enums, ["Provider", "ProviderClass", "UpstreamClient"]);
        assert!(names.enums["Provider"].contains("Sglang"));
    }

    #[test]
    fn comparing_the_provider_name_is_refused_with_full_messages() {
        let found = engine(
            r#"fn f(d: &Deployment) -> bool { d.provider == "anthropic" || "OpenAI" != d.provider }"#,
        );
        assert_eq!(
            found,
            [
                "provider switch in the engine: inference/attempt.rs :: f :: provider id literal \"OpenAI\" (x1)",
                "provider switch in the engine: inference/attempt.rs :: f :: provider id literal \"anthropic\" (x1)",
            ]
        );
    }

    #[test]
    fn matching_and_byte_strings_are_refused() {
        let found = engine(
            r#"fn f(p: &str) { match p { "bedrock" => {}, _ => {} } let _ = matches!(p, "ollama" | "vllm"); let _ = p.as_bytes() == b"openai"; }"#,
        );
        for id in ["bedrock", "ollama", "vllm", "openai"] {
            assert!(found.iter().any(|p| p.contains(id)), "{id} in {found:?}");
        }
    }

    #[test]
    fn model_family_prefixes_and_searches_are_refused() {
        for code in [
            r#"fn f(m: &str) -> bool { m.starts_with("claude-3") }"#,
            r#"fn f(m: &str) -> bool { m == "gpt-4o" }"#,
            r#"fn f(m: &str) -> bool { m.contains("anthropic") }"#,
            r#"fn f(m: &str) -> bool { m.starts_with("anthropic/") }"#,
            r#"fn f(m: &str) -> bool { m.to_lowercase().contains("bedrock") }"#,
            r#"fn f(m: &str) -> bool { m.strip_prefix("anthropic.").is_some() }"#,
            r#"fn f(m: &str) -> bool { m.find("Claude").is_some() }"#,
            r#"fn f(m: &str) -> bool { m.eq_ignore_ascii_case("OpenRouter") }"#,
        ] {
            let found = engine(code);
            assert!(!found.is_empty(), "{code}");
        }
        assert!(
            engine(
                r#"fn f(m: &str) -> bool { m.contains("temperature") || m.starts_with("model") }"#
            )
            .is_empty()
        );
    }

    #[test]
    fn naming_an_adapter_type_or_provider_module_is_refused() {
        let found = engine(
            "use crate::providers::anthropic::AnthropicAdapter;\n\
             use crate::providers::{local::LocalAdapter, openai};\n\
             use crate::providers::messages::ProviderClass;\n\
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
            "provider module providers::messages",
        ] {
            assert!(
                found.iter().any(|p| p.contains(what)),
                "{what} in {found:?}"
            );
        }
    }

    #[test]
    fn the_engine_may_take_only_the_adapter_trait_and_the_registry_from_providers() {
        let found = engine(
            "use crate::providers::{ProviderAdapter, ProviderRegistry};\n\
             use crate::providers::capabilities::Table;\n\
             fn f(r: &crate::providers::Other) {}",
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|p| {
            p.contains("(the engine may take only ProviderAdapter and ProviderRegistry)")
        }));
        // Outside the engine the same imports are fine.
        let names = fixture_names();
        let other = problems(
            &[parse_fixture(
                "governance.rs",
                "use crate::providers::capabilities::Table;",
            )],
            &names,
            &[],
        );
        assert!(other.is_empty(), "{other:?}");
    }

    #[test]
    fn every_provider_kind_enum_variant_is_refused_also_through_aliases_and_globs() {
        for (code, what) in [
            (
                "fn f() { let _ = telemetry::Provider::Anthropic; }",
                "Provider::Anthropic",
            ),
            (
                "fn f() { let _ = ProviderClass::Bedrock; }",
                "ProviderClass::Bedrock",
            ),
            (
                "fn f(c: UpstreamClient) -> bool { matches!(c, UpstreamClient::Local) }",
                "UpstreamClient::Local",
            ),
            (
                "use telemetry::Provider as P;\nfn f() { let _ = P::Anthropic; }",
                "Provider::Anthropic",
            ),
            (
                "use crate::providers::messages::ProviderClass as C;\nfn f() { let _ = C::OpenAi; }",
                "ProviderClass::OpenAi",
            ),
            (
                "use crate::telemetry::Provider::*;",
                "glob import of the provider-kind enum Provider",
            ),
            (
                "use crate::egress_upstream::UpstreamClient::*;",
                "glob import of the provider-kind enum UpstreamClient",
            ),
            (
                "use crate::telemetry::Provider::{Anthropic, OpenAi};",
                "Provider::Anthropic",
            ),
            (
                "use crate::telemetry::Provider::Anthropic as A;",
                "Provider::Anthropic",
            ),
            (
                "fn f() { let _ = matches!(c, Provider::Vllm); }",
                "Provider::Vllm",
            ),
        ] {
            let found = engine(code);
            assert!(
                found.iter().any(|p| p.contains(what)),
                "{what} in {found:?} for {code}"
            );
        }
        // The lookup table and an unrelated enum with a shared variant name pass.
        let found = engine(
            "fn f(x: &str) -> Option<telemetry::Provider> { telemetry::Provider::ALL.iter().copied().find(|p| p.as_str() == x) }\n\
             fn g() { let _ = Color::Anthropic; }",
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
    fn adjacent_files_are_held_to_the_exact_counted_allowlist() {
        let names = fixture_names();
        let source = parse_fixture(
            "governance.rs",
            r#"fn admit(d: &D) { if d.provider == "anthropic" {} }"#,
        );
        let sources = [source];
        let found = problems(&sources, &names, &[]);
        assert_eq!(
            found,
            [
                "provider switch in admission/routing: governance.rs :: admit :: provider id literal \"anthropic\" (x1)"
            ]
        );
        let entry = (
            "governance.rs",
            "admit",
            "provider id literal \"anthropic\"",
        );
        assert!(problems(&sources, &names, &[(entry.0, entry.1, entry.2, 1)]).is_empty());
        let stale = [("governance.rs", "other", entry.2, 1)];
        assert_eq!(problems(&sources, &names, &stale).len(), 2);
        // A second identical hit exceeds the count; a lower count is stale.
        let twice = parse_fixture(
            "governance.rs",
            r#"fn admit(d: &D) { if d.provider == "anthropic" && e.provider == "anthropic" {} }"#,
        );
        assert_eq!(
            problems(&[twice], &names, &[(entry.0, entry.1, entry.2, 1)]),
            [
                "provider switch in admission/routing: governance.rs :: admit :: provider id literal \"anthropic\" found x2, allowed x1"
            ]
        );
    }

    #[test]
    fn a_const_initialiser_is_named_by_the_const() {
        let names = fixture_names();
        let found = problems(
            &[parse_fixture(
                "jobs/schedule.rs",
                r#"pub const PROFILES: [&str; 2] = ["vllm", "openai_compatible"];"#,
            )],
            &names,
            &[],
        );
        assert!(
            found.iter().all(|p| p.contains(":: PROFILES ::")),
            "{found:?}"
        );
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn engine_hits_cannot_be_allowlisted() {
        let names = fixture_names();
        let source = parse_fixture(
            "inference/mod.rs",
            r#"fn f(p: &str) { if p == "openai" {} }"#,
        );
        let allowed = [("inference/mod.rs", "f", "provider id literal \"openai\"", 1)];
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
        assert!(
            found.len() >= FLOOR_IDS.len() + FLOOR_MODULES.len() + FLOOR_ENUMS.len(),
            "{found:?}"
        );
    }
}
