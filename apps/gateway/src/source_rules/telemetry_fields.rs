//! Direct sources of operational event fields and Prometheus family labels.
//! Keep literals intact: spaces inside a fixed label are part of its value.
use std::collections::BTreeSet;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::{self, Visit};

use super::tree::{Scope, Source, flat, parse_fixture, production};

struct Finder<'a> {
    path: &'a str,
    scope: Scope,
    entries: Vec<String>,
    // Local patterns and their initializers in the metric implementation.
    bindings: Vec<(String, BTreeSet<String>, String)>,
    label_names: BTreeSet<(String, String)>,
}

// One physical line per entry, including when a literal spans source lines.
fn token_text(value: &impl ToTokens) -> String {
    value
        .to_token_stream()
        .to_string()
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn identifiers(tokens: &impl ToTokens) -> BTreeSet<String> {
    flat(tokens.to_token_stream())
        .into_iter()
        .filter_map(|t| match t {
            TokenTree::Ident(i) => Some(i.to_string()),
            _ => None,
        })
        .collect()
}

impl Finder<'_> {
    fn record(&mut self, kind: &str, value: &impl ToTokens) {
        self.entries.push(format!(
            "{} :: {} :: {kind} :: {}",
            self.path,
            self.scope.path(),
            token_text(value)
        ));
    }

    fn binding(&mut self, pat: &syn::Pat, value: &syn::Expr) {
        if self.path == "metrics.rs"
            || self.path.starts_with("metrics/")
            || (self.path == "http.rs" && self.scope.path() == "request_context")
        {
            self.bindings.push((
                self.scope.path(),
                identifiers(pat),
                format!("{} = {}", token_text(pat), token_text(value)),
            ));
        }
    }

    // Direct METRICS calls also occur in select!, stream! and macro_rules!.
    fn macro_calls(&mut self, stream: TokenStream) {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (at, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                self.macro_calls(group.stream());
            }
            if let [
                TokenTree::Ident(receiver),
                TokenTree::Punct(dot),
                TokenTree::Ident(_),
                TokenTree::Group(args),
                ..,
            ] = &tokens[at..]
                && receiver == "METRICS"
                && dot.as_char() == '.'
                && args.delimiter() == Delimiter::Parenthesis
            {
                self.record(
                    "macro metrics call",
                    &tokens[at..at + 4].iter().cloned().collect::<TokenStream>(),
                );
            }
        }
    }

    fn label_inputs(&mut self, args: &impl ToTokens) {
        self.label_names.extend(
            identifiers(args)
                .into_iter()
                .map(|name| (self.scope.path(), name)),
        );
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    track_scope!();

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Some(init) = &local.init {
            self.binding(&local.pat, &init.expr);
        }
        visit::visit_local(self, local);
    }

    fn visit_expr_let(&mut self, expr: &'ast syn::ExprLet) {
        self.binding(&expr.pat, &expr.expr);
        visit::visit_expr_let(self, expr);
    }

    fn visit_expr_for_loop(&mut self, expr: &'ast syn::ExprForLoop) {
        self.binding(&expr.pat, &expr.expr);
        visit::visit_expr_for_loop(self, expr);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "get_or_create" || call.method == "get_or_create_owned" {
            self.record("family", call);
            self.label_inputs(&call.args);
        } else if call.method == "phase" {
            self.record("phase label", call);
        } else if let syn::Expr::Path(p) = &*call.receiver
            && p.path.segments.last().is_some_and(|s| s.ident == "METRICS")
        {
            self.record("metrics call", call);
            if call.method == "observe_http" {
                self.label_inputs(&call.args);
            }
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*call.func
            && let Some(name) = p.path.segments.last()
        {
            if name.ident == "emit"
                && (p.path.is_ident("emit") || p.path.segments.iter().any(|s| s.ident == "ops_log"))
            {
                self.record("event", call);
            }
            if self.path == "ops_log.rs" && name.ident == "log_record" {
                self.record("record inputs", call);
            }
            if self.path == "metrics/registry.rs" && name.ident == "series" {
                self.record("precreated labels", call);
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        if self.path == "ops_log.rs" && expr.path.is_ident("Queued") {
            self.record("queued event", expr);
        }
        visit::visit_expr_struct(self, expr);
    }

    fn visit_expr_assign(&mut self, expr: &'ast syn::ExprAssign) {
        if self.path == "metrics_export.rs"
            && let syn::Expr::Index(index) = &*expr.left
            && let syn::Expr::Lit(key) = &*index.index
            && let syn::Lit::Str(key) = &key.lit
            && key.value() == "attributes"
        {
            self.record("OTLP labels", expr);
        }
        visit::visit_expr_assign(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // syn leaves macro bodies opaque. Pin the tokens of a macro containing
        // a producer or family access rather than silently omitting that site.
        self.macro_calls(mac.tokens.clone());
        if (self.path == "ops_log.rs" && mac.path.is_ident("fixed_enum"))
            || identifiers(&mac.tokens).iter().any(|id| {
                matches!(
                    id.as_str(),
                    "OpsEvent" | "ops_log" | "get_or_create" | "get_or_create_owned"
                )
            })
        {
            self.record("macro", mac);
        }
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        if self.path == "ops_log.rs" && item.ident == "OpsEvent" {
            for variant in &item.variants {
                self.record("event variant", variant);
            }
        }
        visit::visit_item_enum(self, item);
    }

    fn visit_impl_item_const(&mut self, item: &'ast syn::ImplItemConst) {
        if self.path == "ops_log.rs"
            && self.scope.path() == "impl OpsEvent"
            && item.ident == "SLUGS"
        {
            self.record("event slugs", &item.expr);
        }
        visit::visit_impl_item_const(self, item);
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let scope = self.scope.path();
        let wire = self.path == "ops_log.rs"
            && (matches!(
                scope.as_str(),
                "log_record"
                    | "impl Exporter::envelope"
                    | "impl ExportFailure::as_str"
                    | "impl Severity::text"
                    | "impl Severity::number"
            ) || scope.starts_with("impl OpsEvent::"));
        let label_helper = matches!(
            (self.path, scope.as_str()),
            (
                "metrics/requests.rs",
                "impl Metrics::labels" | "method_label"
            ) | (
                "metrics.rs",
                "bounded" | "denial_scope" | "admission_outcome"
            )
        );
        if wire || label_helper {
            for (i, stmt) in block.stmts.iter().enumerate() {
                self.record(&format!("statement {}", i + 1), stmt);
            }
            return;
        }
        visit::visit_block(self, block);
    }
}

fn inventory(source: &Source) -> Vec<String> {
    let mut finder = Finder {
        path: &source.path,
        scope: Scope::default(),
        entries: Vec::new(),
        bindings: Vec::new(),
        label_names: BTreeSet::new(),
    };
    finder.visit_file(&source.file);
    for (scope, names, binding) in finder.bindings {
        if names
            .into_iter()
            .any(|name| finder.label_names.contains(&(scope.clone(), name)))
        {
            finder.entries.push(format!(
                "{} :: {scope} :: binding :: {binding}",
                source.path
            ));
        }
    }
    finder.entries
}

#[test]
fn fields_match_reviewed_snapshot() {
    let mut entries: Vec<_> = ["lib.rs", "main.rs"]
        .into_iter()
        .flat_map(production)
        .flat_map(|source| inventory(&source))
        .collect();
    entries.sort();
    let snapshot = include_str!("telemetry_fields.snapshot");
    assert!(!entries.is_empty(), "no telemetry fields found");
    let changes = super::snapshot_diff(entries, snapshot);
    assert!(
        changes.is_empty(),
        "Review the changed telemetry fields and their sources before updating telemetry_fields.snapshot:\n{}",
        changes.join("\n")
    );
}

#[test]
fn additions_and_source_substitutions_change_the_inventory() {
    for (path, source, from, to) in [
        (
            "ops_log.rs",
            "enum OpsEvent { Demo { kind: Kind } }",
            "kind: Kind",
            "kind: Kind, mode: Mode",
        ),
        (
            "worker.rs",
            "fn run() { ops_log::emit(OpsEvent::Demo { kind: first }); }",
            "first",
            "second",
        ),
        (
            "ops_log.rs",
            r#"impl OpsEvent { fn attributes(&self) -> Labels { vec![("kind", self.first())] } }"#,
            "first",
            "second",
        ),
        (
            "ops_log.rs",
            r#"fn log_record() { record["traceId"] = json!(first); }"#,
            "first",
            "second",
        ),
        (
            "metrics.rs",
            r#"fn count() { family.get_or_create(&[("kind", first)]); }"#,
            "first)",
            "first), (\"mode\", second)",
        ),
        (
            "metrics.rs",
            r#"fn count() { family.get_or_create(&[("kind", first)]); }"#,
            "first",
            "second",
        ),
        (
            "metrics.rs",
            r#"fn count() { let labels = [("kind", first)]; family.get_or_create(&labels); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn create() { series(&family, "kind", |i| first[i]); }"#,
            "first",
            "second",
        ),
        (
            "worker.rs",
            r#"fn run() { select! { _ = tick() => METRICS.observe_cache("one two", "ok") } }"#,
            "one two",
            "onetwo",
        ),
    ] {
        let before = inventory(&parse_fixture(path, source));
        let after = inventory(&parse_fixture(path, &source.replace(from, to)));
        assert_ne!(before, after, "{path}: {from} -> {to}");
    }
}

#[test]
fn test_only_producers_are_excluded() {
    let source = parse_fixture(
        "metrics.rs",
        r#"#[cfg(test)] fn count() { family.get_or_create(&[("kind", "demo")]); }"#,
    );
    assert!(inventory(&source).is_empty());
}
