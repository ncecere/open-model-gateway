//! Money is never a float (AGENTS.md invariant, #591): monetary values are
//! integer micro-USD, and a float rounds. Clippy cannot scope
//! `disallowed-types` to a module, so this rule is a source scan: it parses
//! the production build (see [`super::tree`]) and finds the float types
//! (`f32`, `f64`, also as `f64::from`, `as f64`, `parse::<f64>()`), float
//! literals (`0.5`, `1e3`, `2f64`) and SQL float spellings (`float8`,
//! `float4`, `::float`, `as float`, `float(n)`, `as real`, `::real`,
//! `double precision` with any whitespace) in two places:
//!
//! 1. every file whose path names a money concept ([`MONEY_PATHS`]:
//!    governance, billing, pricing, rates, metering, cost, spend, budget,
//!    ledger, rollup, reconcile ...), whole file;
//! 2. any other function, struct, enum, constant or static that also names a
//!    money word ([`MONEY_WORDS`]: `micro`, `microusd`, `micros`, `usd`,
//!    `cost`, `price`, `spend`, `budget`, `charge`, `billing`, `invoice`, ...,
//!    split at `_` and case changes) as an identifier, and the methods of an
//!    `impl`/`trait` whose self type or trait name has such a word: money
//!    computed in an unlikely place. An inline `mod` named for a money
//!    concept makes its contents scope 1;
//! 3. the SQL migrations (`enterprise_migrations/*.sql`): any float column
//!    type, with comments and string literals removed.
//!
//! Anything found must be on [`ALLOWED`] or [`ALLOWED_MIGRATIONS`], one entry
//! per (file, item, kind) with its reason; the list does not grow silently.
//!
//! Cannot see: a type alias defined outside both scopes (`type Amount = f64`
//! elsewhere, used in money code); a `macro_rules!` defined elsewhere whose
//! expansion holds a float; a money float in a function that names no money
//! word in a file that names no money concept; integer rounding mistakes;
//! floats in dependencies.
use std::collections::{BTreeMap, BTreeSet};

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
    "micro", "microusd", "micros", "usd", "cost", "costs", "price", "prices", "priced", "pricing",
    "spend", "spent", "budget", "budgets", "charge", "charged", "billing", "invoice",
];

/// (file, item, kind, count): reviewed floats in money-bearing code, each
/// occurrence counted, so a second `f64` in the same item fails. A new entry
/// needs its reason here; the list does not grow silently.
///
/// * `management/governance/suggestion.rs`, `discounted`: `Number::as_f64`
///   and the literal `0.0` only decide whether a catalog "discount" value is
///   nonzero, to warn that it was not applied. No amount is computed and no
///   float reaches a price, hold or total.
/// * `management/governance/suggestion/limits.rs`, `visit_f64`: the Serde
///   visitor discards numbers while checking JSON complexity before allocating
///   a value tree. The required callback name and its parameter type are the
///   two occurrences. No arithmetic, rate conversion or output uses this value.
/// * `inference/types.rs`, `MessagesOptions`: the struct has a `task_budget`
///   (a token budget, not money, which is why the money-word rule reached it)
///   next to the `top_p` sampling parameter, an `f64`. Granularity is the
///   whole struct, but the count is exact: a second `f64` in it fails.
const ALLOWED: &[(&str, &str, &str, usize)] = &[
    (
        "management/governance/suggestion.rs",
        "discounted",
        "f64",
        1,
    ),
    (
        "management/governance/suggestion.rs",
        "discounted",
        "float literal",
        1,
    ),
    (
        "management/governance/suggestion/limits.rs",
        "impl Check::visit_f64",
        "f64",
        2,
    ),
    ("inference/types.rs", "MessagesOptions", "f64", 1),
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

/// The float spellings in SQL text: `float4`/`float8`, `::float`, `as float`,
/// `float(n)`, `::real`, `as real`, and `double precision` with any
/// whitespace between the words. Words are read as `[a-z0-9_]+`.
fn sql_float(text: &str) -> Vec<&'static str> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                at += 1;
            }
            spans.push((start, at));
        } else {
            at += 1;
        }
    }
    let mut out = Vec::new();
    for (n, &(start, end)) in spans.iter().enumerate() {
        let word = &lower[start..end];
        let before = lower[..start].trim_end();
        let cast = before.ends_with("::")
            || n.checked_sub(1).map(|p| spans[p]).is_some_and(|(ps, pe)| {
                &lower[ps..pe] == "as" && lower[pe..start].trim().is_empty()
            });
        let paren = lower[end..].trim_start().starts_with('(');
        let kind = match word {
            "float4" => Some("SQL float4"),
            "float8" => Some("SQL float8"),
            "float" if cast || paren => Some("SQL float"),
            "real" if cast => Some("SQL real"),
            "double"
                if spans.get(n + 1).is_some_and(|&(ns, ne)| {
                    &lower[ns..ne] == "precision" && lower[end..ns].trim().is_empty()
                }) =>
            {
                Some("SQL double precision")
            }
            _ => None,
        };
        out.extend(kind);
    }
    out
}

/// Text of a migration without comments and string literals.
fn strip_sql(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let next = chars.get(at + 1).copied();
        if c == '-' && next == Some('-') {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
        } else if c == '/' && next == Some('*') {
            at += 2;
            while at < chars.len() && !(chars[at] == '*' && chars.get(at + 1) == Some(&'/')) {
                at += 1;
            }
            at += 2;
        } else if c == '\'' {
            at += 1;
            while at < chars.len() {
                if chars[at] == '\'' {
                    if chars.get(at + 1) == Some(&'\'') {
                        at += 2;
                        continue;
                    }
                    break;
                }
                at += 1;
            }
            at += 1;
            out.push(' ');
        } else {
            out.push(c);
            at += 1;
        }
    }
    out
}

/// Float column types and casts anywhere in a migration: `float`, `float4`,
/// `float8`, `real` and `double precision` as words.
fn migration_floats(sql: &str) -> BTreeSet<&'static str> {
    let text = strip_sql(sql).to_ascii_lowercase();
    let mut found = BTreeSet::new();
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    for (n, word) in words.iter().enumerate() {
        match *word {
            "float" => found.insert("SQL float"),
            "float4" => found.insert("SQL float4"),
            "float8" => found.insert("SQL float8"),
            "real" => found.insert("SQL real"),
            "double" if words.get(n + 1) == Some(&"precision") => {
                found.insert("SQL double precision")
            }
            _ => false,
        };
    }
    found
}

/// What one item (a function, struct, ...) holds, for rule 2.
#[derive(Default)]
struct Frame {
    money: bool,
    kinds: BTreeMap<String, usize>,
}

struct Finder {
    file: String,
    whole_file: bool,
    scope: Scope,
    /// Outermost item being read, and its scope path.
    frame: Option<(String, Frame)>,
    hits: BTreeMap<Hit, usize>,
}

impl Finder {
    fn float(&mut self, kind: String) {
        if self.whole_file {
            *self
                .hits
                .entry(Hit {
                    file: self.file.clone(),
                    item: self.scope.path(),
                    kind,
                })
                .or_default() += 1;
        } else if let Some((_, frame)) = &mut self.frame {
            *frame.kinds.entry(kind).or_default() += 1;
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
            for (kind, count) in frame.kinds {
                *self
                    .hits
                    .entry(Hit {
                        file: self.file.clone(),
                        item: item.clone(),
                        kind,
                    })
                    .or_default() += count;
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
        for kind in sql_float(&lit.value()) {
            self.float(kind.to_owned());
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
                            for kind in sql_float(&s.value()) {
                                self.float(kind.to_owned());
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
pub(super) fn scan(source: &Source) -> BTreeMap<Hit, usize> {
    let whole_file = money_path(&source.path);
    let mut finder = Finder {
        file: source.path.clone(),
        whole_file,
        scope: Scope::default(),
        frame: None,
        hits: BTreeMap::new(),
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
                let name = module.ident.unraw().to_string();
                let was = finder.whole_file;
                finder.whole_file |= money_path(&name);
                finder.scope.enter(name);
                for inner in items {
                    scan_item(finder, inner);
                }
                finder.scope.leave();
                finder.whole_file = was;
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
                finder.word(&super::tree::type_name(&imp.self_ty));
                if let Some((trait_path, _)) = &imp.trait_
                    && let Some(last) = trait_path.segments.last()
                {
                    finder.word(&last.ident.unraw().to_string());
                }
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
                finder.word(&tr.ident.unraw().to_string());
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

/// Floats not on [`ALLOWED`] (or more of them than the entry counts), and
/// allowlist entries that match nothing.
pub(super) fn problems(sources: &[Source], allowed: &[(&str, &str, &str, usize)]) -> Vec<String> {
    let mut found: BTreeMap<Hit, usize> = BTreeMap::new();
    for source in sources {
        for (hit, n) in scan(source) {
            *found.entry(hit).or_default() += n;
        }
    }
    let mut allowed_hits: BTreeMap<Hit, usize> = BTreeMap::new();
    for (file, item, kind, count) in allowed {
        *allowed_hits
            .entry(Hit {
                file: (*file).to_owned(),
                item: (*item).to_owned(),
                kind: (*kind).to_owned(),
            })
            .or_default() += count;
    }
    let mut out = Vec::new();
    for (hit, n) in &found {
        match allowed_hits.get(hit) {
            Some(want) if want == n => {}
            Some(want) => out.push(format!(
                "float in money code: {} found x{n}, allowed x{want}",
                hit.text()
            )),
            None => out.push(format!("float in money code: {} (x{n})", hit.text())),
        }
    }
    for (hit, want) in &allowed_hits {
        if !found.contains_key(hit) {
            out.push(format!("stale allowlist entry: {} (x{want})", hit.text()));
        }
    }
    out
}

/// (file, kind): float column types in migrations. None today.
const ALLOWED_MIGRATIONS: &[(&str, &str)] = &[];

/// Float types in `(file name, SQL text)` migrations, minus the allowlist.
pub(super) fn migration_problems(
    files: &[(String, String)],
    allowed: &[(&str, &str)],
) -> Vec<String> {
    let mut found = BTreeSet::new();
    for (name, sql) in files {
        for kind in migration_floats(sql) {
            found.insert((name.clone(), kind.to_owned()));
        }
    }
    let allowed: BTreeSet<(String, String)> = allowed
        .iter()
        .map(|(f, k)| ((*f).to_owned(), (*k).to_owned()))
        .collect();
    let mut out: Vec<String> = found
        .difference(&allowed)
        .map(|(f, k)| format!("float column type in migration {f}: {k}"))
        .collect();
    out.extend(
        allowed
            .difference(&found)
            .map(|(f, k)| format!("stale migration allowlist entry: {f} {k}")),
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
        let found = problems(&[source], &[("governance/pricing.rs", "price", "f64", 1)]);
        assert_eq!(
            found,
            vec!["stale allowlist entry: governance/pricing.rs :: price :: f64 (x1)".to_owned()]
        );
    }

    #[test]
    fn an_allowlisted_float_passes_only_at_its_exact_item() {
        let code = "pub fn ratio() -> f64 { 0.5 }\npub fn other() -> f64 { 0.5 }";
        let source = parse_fixture("governance/pricing.rs", code);
        let found = problems(
            &[source],
            &[
                ("governance/pricing.rs", "ratio", "f64", 1),
                ("governance/pricing.rs", "ratio", "float literal", 1),
            ],
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|p| p.contains(":: other ::")));
    }

    #[test]
    fn a_second_float_in_an_allowlisted_item_fails() {
        let allowed = [
            ("governance/pricing.rs", "Options", "f64", 1),
            ("governance/pricing.rs", "Options", "float literal", 0),
        ];
        let one = parse_fixture(
            "governance/pricing.rs",
            "pub struct Options { pub top_p: Option<f64> }",
        );
        assert!(problems(&[one], &allowed[..1]).is_empty());
        let two = parse_fixture(
            "governance/pricing.rs",
            "pub struct Options { pub top_p: Option<f64>, pub cost_ratio: f64 }",
        );
        assert_eq!(
            problems(&[two], &allowed[..1]),
            ["float in money code: governance/pricing.rs :: Options :: f64 found x2, allowed x1"]
        );
        // And an allowance larger than what is there is stale-by-count.
        let one = parse_fixture(
            "governance/pricing.rs",
            "pub struct Options { pub top_p: Option<f64> }",
        );
        assert_eq!(
            problems(&[one], &[("governance/pricing.rs", "Options", "f64", 2)]),
            ["float in money code: governance/pricing.rs :: Options :: f64 found x1, allowed x2"]
        );
    }

    #[test]
    fn sql_spellings_in_every_form_are_refused() {
        for (sql, kind) in [
            ("SELECT x::float4 FROM t", "SQL float4"),
            ("SELECT CAST(x AS float8)", "SQL float8"),
            ("SELECT x::float FROM t", "SQL float"),
            ("SELECT CAST(x AS FLOAT)", "SQL float"),
            ("SELECT CAST(x AS float(53))", "SQL float"),
            ("SELECT x::real", "SQL real"),
            ("SELECT CAST(x as   real)", "SQL real"),
            ("SELECT x::double precision", "SQL double precision"),
            ("SELECT x::DOUBLE\n\t PRECISION", "SQL double precision"),
        ] {
            assert_eq!(sql_float(sql), [kind], "{sql:?}");
            let found = fixture(
                "governance/pricing.rs",
                &format!("pub fn q() {{ let _ = sqlx::query({sql:?}); }}"),
            );
            assert!(
                found.iter().any(|p| p.contains(kind)),
                "{sql:?} in {found:?}"
            );
        }
        for sql in [
            "a real cost",
            "floating point",
            "SELECT float_col",
            "x::numeric",
            "double entry",
        ] {
            assert!(sql_float(sql).is_empty(), "{sql:?}");
        }
    }

    #[test]
    fn sql_inside_a_macro_is_refused() {
        let found = fixture(
            "billing/v3.rs",
            r#"pub fn a() { let _ = sqlx::query_scalar!("SELECT sum(x)::float8 FROM t"); }
               pub fn b() { let _ = format!("SELECT {}::double precision", 1); }"#,
        );
        assert!(
            found.iter().any(|p| p.contains(":: a :: SQL float8")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains(":: b :: SQL double precision")),
            "{found:?}"
        );
    }

    #[test]
    fn methods_of_a_money_type_or_trait_are_money_code() {
        let found = fixture(
            "inference/anywhere.rs",
            "pub struct SpendWindow;\n\
             impl SpendWindow { pub fn ratio(&self) -> f64 { 0.5 } }\n\
             pub trait PriceSource { fn quote(&self) -> f64 { 1.5 } }\n\
             impl PriceSource for Other { fn quote(&self) -> f64 { 2.5 } }\n\
             impl Other { fn plain(&self) -> f64 { 3.5 } }",
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains(":: impl SpendWindow::ratio :: f64")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains(":: trait PriceSource::quote :: f64")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains(":: impl Other::quote :: f64")),
            "{found:?}"
        );
        assert!(!found.iter().any(|p| p.contains("::plain ::")), "{found:?}");
    }

    #[test]
    fn nested_modules_consts_and_statics_are_read() {
        let found = fixture(
            "inference/anywhere.rs",
            "mod inner { pub const MICRO_RATE: f64 = 0.5; pub static USD_CAP: f32 = 1.0; }\n\
             mod pricing { pub fn any_name() -> f64 { 1.0 } }\n\
             mod other { pub fn any_name() -> f64 { 1.0 } }",
        );
        assert!(
            found.iter().any(|p| p.contains("inner::MICRO_RATE :: f64")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|p| p.contains("inner::USD_CAP :: f32")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|p| p.contains("pricing::any_name :: f64")),
            "{found:?}"
        );
        assert!(!found.iter().any(|p| p.contains("other::")), "{found:?}");
    }

    #[test]
    fn the_repository_money_words_trigger() {
        for name in [
            "held_microusd",
            "used_microusd",
            "provider_cost_microusd",
            "elapsed_micros",
            "MicroUsd",
        ] {
            assert!(money_word(name), "{name}");
        }
        let found = fixture(
            "inference/anywhere.rs",
            "pub fn used(held_microusd: i64) -> i64 { (held_microusd as f64 * 0.5) as i64 }",
        );
        assert!(
            found.iter().any(|p| p.contains(":: used :: f64")),
            "{found:?}"
        );
    }

    #[test]
    fn the_reconcile_path_rule_and_word_boundaries() {
        assert!(money_path("management/provider_costs.rs"));
        assert!(money_path("governance/reconcile.rs"));
        assert!(money_path("governance/reconcile_token_tests.rs"));
        assert!(money_path("x/reconciliation.rs"));
        assert!(!money_path("inference/generates.rs"));
        assert!(!money_path("providers/surprise.rs"));
        let found = fixture("governance/reconcile.rs", "pub fn any() -> f64 { 0.0 }");
        assert!(!found.is_empty());
    }

    #[test]
    fn float_columns_in_migrations_are_refused() {
        let files = |sql: &str| vec![("0900_x.sql".to_owned(), sql.to_owned())];
        for (sql, kind) in [
            ("CREATE TABLE t (a float);", "SQL float"),
            ("CREATE TABLE t (a FLOAT8);", "SQL float8"),
            ("ALTER TABLE t ADD b float4;", "SQL float4"),
            ("CREATE TABLE t (a real);", "SQL real"),
            (
                "CREATE TABLE t (a double   precision);",
                "SQL double precision",
            ),
            (
                "CREATE TABLE t (a DOUBLE\nPRECISION);",
                "SQL double precision",
            ),
        ] {
            let found = migration_problems(&files(sql), &[]);
            assert_eq!(
                found,
                [format!("float column type in migration 0900_x.sql: {kind}")],
                "{sql}"
            );
        }
        for sql in [
            "CREATE TABLE t (a bigint, b numeric(20,6)); -- not a float, or real\n/* double precision */",
            "INSERT INTO t VALUES ('a real float');",
        ] {
            assert!(migration_problems(&files(sql), &[]).is_empty(), "{sql}");
        }
        let allowed = [("0900_x.sql", "SQL float")];
        assert!(migration_problems(&files("CREATE TABLE t (a float);"), &allowed).is_empty());
        assert_eq!(migration_problems(&files("SELECT 1;"), &allowed).len(), 1);
    }

    #[test]
    fn the_migrations_hold_no_unreviewed_float_column() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("enterprise_migrations");
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "sql") {
                files.push((
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read_to_string(&path).unwrap(),
                ));
            }
        }
        assert!(files.len() > 50, "only {} migrations read", files.len());
        let found = migration_problems(&files, ALLOWED_MIGRATIONS);
        assert!(found.is_empty(), "{}", found.join("\n"));
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
