//! Money is never a float (AGENTS.md invariant, #591): monetary values are
//! integer micro-USD, and a float rounds. Clippy cannot scope
//! `disallowed-types` to a module, so this rule is a source scan: it parses
//! the production build (see [`super::tree`]) and finds the float types
//! (`f32`, `f64`, also as `f64::from`, `as f64`, `parse::<f64>()`), float
//! literals (`0.5`, `1e3`, `2f64`) and SQL float casts (`float8`, `float4`,
//! `double precision`, `::real`) in two places:
//!
//! 1. every file whose path names a money concept ([`MONEY_PATHS`]:
//!    governance, billing, pricing, rates, metering, cost, spend, budget,
//!    ledger, rollup, reconcile ...), whole file;
//! 2. any other function, struct, enum, constant or static that also names a
//!    money word (`micro`, `usd`, `cost`, `price`, `spend`, `budget`,
//!    `charge`, `billing`, `invoice`) as an identifier: money computed in an
//!    unlikely place.
//!
//! Anything found must be on [`ALLOWED`], one entry per (file, item, kind)
//! with its reason; the list does not grow silently.
use std::collections::BTreeSet;

use proc_macro2::TokenTree;
use syn::{ext::IdentExt, visit::Visit};

use super::tree::{Scope, Source, flat};

/// A path with any of these words (split at `/`, `.` and `_`; also any word
/// starting `reconcil`) names a money concept.
const MONEY_PATHS: &[&str] = &[
    "governance",
    "billing",
    "pricing",
    "price",
    "prices",
    "rates",
    "metering",
    "cost",
    "costs",
    "spend",
    "budget",
    "budgets",
    "ledger",
    "rollup",
    "rollups",
    "invoice",
];

/// Identifier words (split at `_` and at lower-to-upper case changes) that
/// make an item money-bearing.
const MONEY_WORDS: &[&str] = &[
    "micro", "usd", "cost", "costs", "price", "prices", "priced", "pricing", "spend", "spent",
    "budget", "budgets", "charge", "charged", "billing", "invoice",
];

/// (file, item, kind): reviewed floats in money-bearing code. A new entry
/// needs its reason here; the list does not grow silently.
///
/// * `management/governance/suggestion.rs`, `discounted`: `Number::as_f64`
///   and the literal `0.0` only decide whether a catalog "discount" value is
///   nonzero, to warn that it was not applied. No amount is computed and no
///   float reaches a price, hold or total.
/// * `inference/types.rs`, `MessagesOptions`: the struct has a `task_budget`
///   (a token budget, not money, which is why the money-word rule reached it)
///   next to the `top_p` sampling parameter, an `f64`. Granularity is the
///   whole struct: a new money float in it would pass.
const ALLOWED: &[(&str, &str, &str)] = &[
    ("management/governance/suggestion.rs", "discounted", "f64"),
    (
        "management/governance/suggestion.rs",
        "discounted",
        "float literal",
    ),
    ("inference/types.rs", "MessagesOptions", "f64"),
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Hit {
    pub file: String,
    pub item: String,
    pub kind: String,
}

impl Hit {
    fn text(&self) -> String {
        format!("{} :: {} :: {}", self.file, self.item, self.kind)
    }
}

fn words(ident: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for c in ident.chars() {
        if c == '_' {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower && !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn money_word(ident: &str) -> bool {
    words(ident)
        .iter()
        .any(|w| MONEY_WORDS.contains(&w.as_str()))
}

fn sql_float(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    ["float8", "float4", "double precision", "::real"]
        .into_iter()
        .find(|needle| lower.contains(needle))
}

/// What one item (a function, struct, ...) holds, for rule 2.
#[derive(Default)]
struct Frame {
    money: bool,
    kinds: BTreeSet<String>,
}

struct Finder {
    file: String,
    whole_file: bool,
    scope: Scope,
    /// Outermost item being read, and its scope path.
    frame: Option<(String, Frame)>,
    hits: BTreeSet<Hit>,
}

impl Finder {
    fn float(&mut self, kind: String) {
        if self.whole_file {
            self.hits.insert(Hit {
                file: self.file.clone(),
                item: self.scope.path(),
                kind,
            });
        } else if let Some((_, frame)) = &mut self.frame {
            frame.kinds.insert(kind);
        }
    }

    fn word(&mut self, ident: &str) {
        if let Some((_, frame)) = &mut self.frame {
            frame.money |= money_word(ident);
        }
    }

    fn open(&mut self) -> bool {
        if self.frame.is_none() {
            self.frame = Some((self.scope.path(), Frame::default()));
            true
        } else {
            false
        }
    }

    fn close(&mut self, opened: bool) {
        if !opened {
            return;
        }
        let Some((item, frame)) = self.frame.take() else {
            return;
        };
        if frame.money && !self.whole_file {
            for kind in frame.kinds {
                self.hits.insert(Hit {
                    file: self.file.clone(),
                    item: item.clone(),
                    kind,
                });
            }
        }
    }
}

impl<'ast> Visit<'ast> for Finder {
    fn visit_ident(&mut self, ident: &'ast proc_macro2::Ident) {
        let name = ident.unraw().to_string();
        // `f64`, and `as_f64`, `to_f64`, `from_f64`, `parse_f32`, ...
        if let Some(float) = words(&name).into_iter().find(|w| w == "f32" || w == "f64") {
            self.float(float);
        } else {
            self.word(&name);
        }
    }

    /// `2f64` is an integer token with a float suffix.
    fn visit_lit_int(&mut self, lit: &'ast syn::LitInt) {
        if matches!(lit.suffix(), "f32" | "f64") {
            self.float(lit.suffix().to_owned());
        }
    }

    fn visit_lit_float(&mut self, _: &'ast syn::LitFloat) {
        self.float("float literal".to_owned());
    }

    fn visit_lit_str(&mut self, lit: &'ast syn::LitStr) {
        if let Some(kind) = sql_float(&lit.value()) {
            self.float(format!("SQL {kind}"));
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        syn::visit::visit_macro(self, mac);
        for token in flat(mac.tokens.clone()) {
            match token {
                TokenTree::Ident(ident) => self.visit_ident(&ident),
                TokenTree::Literal(literal) => {
                    match syn::parse_str::<syn::Lit>(&literal.to_string()) {
                        Ok(syn::Lit::Float(_)) => self.float("float literal".to_owned()),
                        Ok(syn::Lit::Int(i)) if matches!(i.suffix(), "f32" | "f64") => {
                            self.float(i.suffix().to_owned());
                        }
                        Ok(syn::Lit::Str(s)) => {
                            if let Some(kind) = sql_float(&s.value()) {
                                self.float(format!("SQL {kind}"));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

fn item_is_framed(item: &syn::Item) -> bool {
    matches!(
        item,
        syn::Item::Fn(_)
            | syn::Item::Struct(_)
            | syn::Item::Enum(_)
            | syn::Item::Const(_)
            | syn::Item::Static(_)
            | syn::Item::Type(_)
    )
}

fn money_path(path: &str) -> bool {
    path.to_ascii_lowercase()
        .split(['/', '.', '_'])
        .any(|word| MONEY_PATHS.contains(&word) || word.starts_with("reconcil"))
}

/// Every float in money code of one file.
pub(super) fn scan(source: &Source) -> BTreeSet<Hit> {
    let whole_file = money_path(&source.path);
    let mut finder = Finder {
        file: source.path.clone(),
        whole_file,
        scope: Scope::default(),
        frame: None,
        hits: BTreeSet::new(),
    };
    // Frames: each outermost item (and each method of an impl or trait) is
    // read on its own, so one money word does not taint its neighbours.
    for item in &source.file.items {
        scan_item(&mut finder, item);
    }
    finder.hits
}

fn scan_item(finder: &mut Finder, item: &syn::Item) {
    match item {
        syn::Item::Mod(module) => {
            if let Some((_, items)) = &module.content {
                finder.scope.enter(module.ident.unraw().to_string());
                for inner in items {
                    scan_item(finder, inner);
                }
                finder.scope.leave();
            }
        }
        syn::Item::Impl(imp) => {
            finder
                .scope
                .enter(format!("impl {}", super::tree::type_name(&imp.self_ty)));
            for member in &imp.items {
                let name = match member {
                    syn::ImplItem::Fn(f) => f.sig.ident.unraw().to_string(),
                    syn::ImplItem::Const(c) => c.ident.unraw().to_string(),
                    syn::ImplItem::Type(t) => t.ident.unraw().to_string(),
                    _ => "<item>".to_owned(),
                };
                finder.scope.enter(name);
                let opened = finder.open();
                finder.visit_impl_item(member);
                finder.close(opened);
                finder.scope.leave();
            }
            finder.scope.leave();
        }
        syn::Item::Trait(tr) => {
            finder.scope.enter(format!("trait {}", tr.ident.unraw()));
            for member in &tr.items {
                let name = match member {
                    syn::TraitItem::Fn(f) => f.sig.ident.unraw().to_string(),
                    syn::TraitItem::Const(c) => c.ident.unraw().to_string(),
                    syn::TraitItem::Type(t) => t.ident.unraw().to_string(),
                    _ => "<item>".to_owned(),
                };
                finder.scope.enter(name);
                let opened = finder.open();
                finder.visit_trait_item(member);
                finder.close(opened);
                finder.scope.leave();
            }
            finder.scope.leave();
        }
        other if item_is_framed(other) => {
            let name = match other {
                syn::Item::Fn(f) => f.sig.ident.unraw().to_string(),
                syn::Item::Struct(s) => s.ident.unraw().to_string(),
                syn::Item::Enum(e) => e.ident.unraw().to_string(),
                syn::Item::Const(c) => c.ident.unraw().to_string(),
                syn::Item::Static(s) => s.ident.unraw().to_string(),
                syn::Item::Type(t) => t.ident.unraw().to_string(),
                _ => String::new(),
            };
            finder.scope.enter(name);
            let opened = finder.open();
            finder.visit_item(other);
            finder.close(opened);
            finder.scope.leave();
        }
        other => finder.visit_item(other),
    }
}

/// Floats not on [`ALLOWED`], and allowlist entries that match nothing.
pub(super) fn problems(sources: &[Source], allowed: &[(&str, &str, &str)]) -> Vec<String> {
    let mut found = BTreeSet::new();
    for source in sources {
        found.extend(scan(source));
    }
    let allowed: BTreeSet<Hit> = allowed
        .iter()
        .map(|(file, item, kind)| Hit {
            file: (*file).to_owned(),
            item: (*item).to_owned(),
            kind: (*kind).to_owned(),
        })
        .collect();
    let mut out: Vec<String> = found
        .difference(&allowed)
        .map(|h| format!("float in money code: {}", h.text()))
        .collect();
    out.extend(
        allowed
            .difference(&found)
            .map(|h| format!("stale allowlist entry: {}", h.text())),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::super::tree::{parse_fixture, production};
    use super::*;

    fn fixture(path: &str, code: &str) -> Vec<String> {
        problems(&[parse_fixture(path, code)], &[])
    }

    #[test]
    fn money_modules_have_no_floats() {
        let mut sources = production("lib.rs");
        sources.extend(production("main.rs"));
        let problems = problems(&sources, ALLOWED);
        assert!(
            problems.is_empty(),
            "money is integer micro-USD, never a float (AGENTS.md, #591). Use i64/u64 micro-USD \
             arithmetic; a ratio or display value needs an ALLOWED entry with its reason in \
             source_rules/money_float.rs:\n{}",
            problems.join("\n")
        );
    }

    #[test]
    fn the_scan_reads_the_money_modules() {
        // Guards the guard: the module tree resolves, and the money paths
        // exist, so an empty result cannot come from an empty scan.
        let sources = production("lib.rs");
        for path in [
            "governance.rs",
            "governance/pricing.rs",
            "billing/v3.rs",
            "providers/metering.rs",
        ] {
            assert!(
                sources
                    .iter()
                    .any(|s| s.path == path && money_path(&s.path)),
                "{path} is not scanned"
            );
        }
        assert!(sources.iter().all(|s| !s.path.ends_with("tests.rs")));
    }

    #[test]
    fn a_float_type_in_a_money_file_is_refused() {
        let found = fixture(
            "governance/pricing.rs",
            "pub fn price(tokens: u64) -> u64 { (tokens as f64 * 1.5) as u64 }",
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains("governance/pricing.rs :: price :: f64"))
        );
        assert!(found.iter().any(|p| p.contains("float literal")));
    }

    #[test]
    fn float_literals_sql_casts_and_macro_bodies_are_refused() {
        let found = fixture(
            "billing/v3.rs",
            r#"
            const RATE: u64 = 3;
            pub fn a() -> f32 { 0.5f32 }
            pub fn b() -> u64 { let x = 2f64; x as u64 }
            pub fn f(n: &Number) -> bool { n.as_f64().is_some() }
            pub fn c() { let _ = sqlx::query("SELECT sum(cost)::float8 FROM t"); }
            pub fn d() { let _ = format!("{}", 1e3); }
            pub fn e() { let _ = vec![0.25]; }
            "#,
        );
        for needle in [
            ":: a :: f32",
            ":: b :: f64",
            ":: f :: f64",
            ":: c :: SQL float8",
            ":: d :: float literal",
            ":: e :: float literal",
        ] {
            assert!(
                found.iter().any(|p| p.contains(needle)),
                "{needle} in {found:?}"
            );
        }
    }

    #[test]
    fn a_float_next_to_a_money_word_outside_the_money_paths_is_refused() {
        let found = fixture(
            "inference/anywhere.rs",
            "pub fn cost_estimate_micro_usd(n: u64) -> u64 { (n as f64 * 0.9) as u64 }\n\
             pub struct Quote { pub price_usd: f64 }",
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains(":: cost_estimate_micro_usd :: f64"))
        );
        assert!(found.iter().any(|p| p.contains(":: Quote :: f64")));
    }

    #[test]
    fn a_float_that_is_not_near_money_is_not_flagged_outside_money_paths() {
        let found = fixture(
            "identity/throttle.rs",
            "pub fn per_second(per_minute: u32) -> f64 { f64::from(per_minute) / 60.0 }",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn test_only_code_and_ordinary_integers_are_ignored() {
        let found = fixture(
            "governance/pricing.rs",
            "pub fn price(n: u64) -> u64 { n * 3 }\n\
             #[cfg(test)] mod tests { fn t() -> f64 { 1.5 } }\n\
             #[test] fn alone() { let _ = 2.5; }",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_allowlist_entry_that_matches_nothing_is_refused() {
        let source = parse_fixture("governance/pricing.rs", "pub fn price() -> u64 { 1 }");
        let found = problems(&[source], &[("governance/pricing.rs", "price", "f64")]);
        assert_eq!(
            found,
            vec!["stale allowlist entry: governance/pricing.rs :: price :: f64".to_owned()]
        );
    }

    #[test]
    fn an_allowlisted_float_passes_only_at_its_exact_item() {
        let code = "pub fn ratio() -> f64 { 0.5 }\npub fn other() -> f64 { 0.5 }";
        let source = parse_fixture("governance/pricing.rs", code);
        let found = problems(
            &[source],
            &[
                ("governance/pricing.rs", "ratio", "f64"),
                ("governance/pricing.rs", "ratio", "float literal"),
            ],
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|p| p.contains(":: other ::")));
    }

    #[test]
    fn words_split_snake_and_camel_case() {
        assert_eq!(words("cost_micro_usd"), ["cost", "micro", "usd"]);
        assert_eq!(words("PriceTier"), ["price", "tier"]);
        assert_eq!(words("as_f64"), ["as", "f64"]);
        assert!(money_word("providerCost"));
        assert!(!money_word("temperature"));
    }
}
