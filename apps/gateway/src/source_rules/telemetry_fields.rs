//! Direct sources of operational event fields and Prometheus family labels.
//! Keep literals intact: spaces inside a fixed label are part of its value.
use std::collections::BTreeSet;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::visit::{self, Visit};

use super::tree::{Scope, Source, flat, parse_fixture, production};

type Reads = BTreeSet<(String, String)>;

struct Finder<'a> {
    path: &'a str,
    scope: Scope,
    entries: Vec<String>,
    reads: Vec<Reads>,
    // Local patterns and their initializers in the metric implementation.
    bindings: Vec<(String, BTreeSet<String>, String)>,
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

    fn scoped(&mut self, visit: impl FnOnce(&mut Self)) {
        let depth = self.bindings.len();
        visit(self);
        self.bindings.truncate(depth);
    }

    fn tracks_bindings(&self) -> bool {
        self.path == "metrics.rs"
            || self.path.starts_with("metrics/")
            || (self.path == "http.rs" && self.scope.path() == "request_context")
    }

    fn read_sources(&mut self, visit: impl FnOnce(&mut Self)) -> Reads {
        self.reads.push(Reads::new());
        visit(self);
        self.reads.pop().unwrap()
    }

    fn binding(&mut self, pat: &syn::Pat, value: Option<&syn::Expr>, reads: Reads) {
        if self.tracks_bindings() {
            #[derive(Default)]
            struct Names(BTreeSet<String>);
            impl Visit<'_> for Names {
                fn visit_pat_ident(&mut self, pat: &syn::PatIdent) {
                    self.0.insert(pat.ident.to_string());
                    visit::visit_pat_ident(self, pat);
                }
            }
            let mut names = Names::default();
            names.visit_pat(pat);
            let mut source = token_text(pat);
            if let Some(value) = value {
                source.push_str(&format!(" = {}", token_text(value)));
                // A shadowing initializer still reads the previous binding.
                for (name, previous) in reads {
                    if names.0.contains(&name) {
                        source.push_str(&format!(" [{previous}]"));
                    }
                }
            }
            self.bindings.push((self.scope.path(), names.0, source));
        }
    }

    fn visible(&self, name: &str) -> Option<&str> {
        let current = self.scope.path();
        self.bindings
            .iter()
            .rev()
            .find(|(scope, names, _)| *scope == current && names.contains(name))
            .map(|(_, _, source)| source.as_str())
    }

    // Direct METRICS calls also occur in select!, stream! and macro_rules!.
    fn macro_calls(&mut self, stream: TokenStream) {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (at, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                self.macro_calls(group.stream());
            }
            if let [TokenTree::Ident(name), TokenTree::Group(args), ..] = &tokens[at..]
                && name == "emit"
                && args.delimiter() == Delimiter::Parenthesis
                && !matches!(&tokens[..at], [.., TokenTree::Punct(dot)] if dot.as_char() == '.')
                && !matches!(&tokens[..at], [.., TokenTree::Punct(a), TokenTree::Punct(b)] if a.as_char() == ':' && b.as_char() == ':')
            {
                self.record(
                    "macro event",
                    &tokens[at..at + 2].iter().cloned().collect::<TokenStream>(),
                );
            }
            if let [
                TokenTree::Ident(receiver),
                TokenTree::Punct(dot),
                TokenTree::Ident(method),
                TokenTree::Group(args),
                ..,
            ] = &tokens[at..]
                && (receiver == "METRICS" || method == "phase")
                && dot.as_char() == '.'
                && args.delimiter() == Delimiter::Parenthesis
            {
                self.record(
                    if method == "phase" {
                        "macro phase label"
                    } else {
                        "macro metrics call"
                    },
                    &tokens[at..at + 4].iter().cloned().collect::<TokenStream>(),
                );
            }
        }
    }

    fn label_use(&mut self, kind: &str, call: &syn::ExprMethodCall) {
        self.record(kind, call);
        let entry = self.entries.len() - 1;
        self.visit_expr(&call.receiver);
        let reads = self.read_sources(|this| {
            for arg in &call.args {
                this.visit_expr(arg);
            }
        });
        for (name, source) in reads {
            self.entries[entry].push_str(&format!(" :: {name} from {source}"));
        }
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    track_scope!();

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if !self.reads.is_empty()
            && let Some(name) = path.path.get_ident()
            && let Some(source) = self.visible(&name.to_string())
        {
            let read = (name.to_string(), source.to_owned());
            for reads in &mut self.reads {
                reads.insert(read.clone());
            }
        }
        visit::visit_expr_path(self, path);
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if !self.tracks_bindings() {
            visit::visit_local(self, local);
            return;
        }
        if let Some(init) = &local.init {
            let reads = self.read_sources(|this| this.visit_expr(&init.expr));
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
            self.binding(&local.pat, Some(&init.expr), reads);
        } else {
            self.binding(&local.pat, None, Reads::new());
        }
    }

    fn visit_expr_let(&mut self, expr: &'ast syn::ExprLet) {
        let reads = self.read_sources(|this| this.visit_expr(&expr.expr));
        self.binding(&expr.pat, Some(&expr.expr), reads);
    }

    fn visit_expr_if(&mut self, expr: &'ast syn::ExprIf) {
        self.scoped(|this| {
            this.visit_expr(&expr.cond);
            this.visit_block(&expr.then_branch);
        });
        if let Some((_, branch)) = &expr.else_branch {
            self.visit_expr(branch);
        }
    }

    fn visit_expr_while(&mut self, expr: &'ast syn::ExprWhile) {
        self.scoped(|this| visit::visit_expr_while(this, expr));
    }

    fn visit_expr_for_loop(&mut self, expr: &'ast syn::ExprForLoop) {
        let reads = self.read_sources(|this| this.visit_expr(&expr.expr));
        self.scoped(|this| {
            this.binding(&expr.pat, Some(&expr.expr), reads);
            this.visit_block(&expr.body);
        });
    }

    fn visit_expr_closure(&mut self, expr: &'ast syn::ExprClosure) {
        self.scoped(|this| {
            for pat in &expr.inputs {
                this.binding(pat, None, Reads::new());
            }
            this.visit_expr(&expr.body);
        });
    }

    fn visit_expr_match(&mut self, expr: &'ast syn::ExprMatch) {
        if !self.tracks_bindings() {
            visit::visit_expr_match(self, expr);
            return;
        }
        for attr in &expr.attrs {
            self.visit_attribute(attr);
        }
        let reads = self.read_sources(|this| this.visit_expr(&expr.expr));
        for arm in &expr.arms {
            self.scoped(|this| {
                this.binding(&arm.pat, Some(&expr.expr), reads.clone());
                this.visit_arm(arm);
            });
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "get_or_create" || call.method == "get_or_create_owned" {
            self.label_use("family", call);
            return;
        } else if call.method == "collection_error"
            && self.scope.path().starts_with("impl Metrics::")
            && matches!(&*call.receiver, syn::Expr::Path(p) if p.path.is_ident("self"))
        {
            self.record("collection error label", call);
        } else if call.method == "phase" {
            self.record("phase label", call);
        } else if let syn::Expr::Path(p) = &*call.receiver
            && p.path.segments.last().is_some_and(|s| s.ident == "METRICS")
        {
            if call.method == "observe_http" {
                self.label_use("metrics call", call);
                return;
            }
            self.record("metrics call", call);
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
        let fixed_values = mac.path.is_ident("fixed_enum")
            && match self.path {
                "ops_log.rs" => true,
                "otlp.rs" | "metrics_export.rs" | "telemetry.rs" => {
                    let expected = if self.path == "otlp.rs" {
                        "Delivery"
                    } else {
                        "DropReason"
                    };
                    matches!(mac.tokens.clone().into_iter().next(), Some(TokenTree::Ident(name)) if name == expected)
                }
                _ => false,
            };
        let names = identifiers(&mac.tokens);
        let family = names.contains("get_or_create") || names.contains("get_or_create_owned");
        if fixed_values || family || names.contains("OpsEvent") || names.contains("ops_log") {
            self.record("macro", mac);
        }
        // Macro syntax is opaque. Alongside a family macro's complete tokens,
        // retain candidate inputs visible at its invocation. A macro in a
        // regular family argument can also read an enclosing binding.
        if family || !self.reads.is_empty() {
            for name in names {
                if let Some(source) = self.visible(&name).map(str::to_owned) {
                    if family {
                        self.entries
                            .last_mut()
                            .unwrap()
                            .push_str(&format!(" :: {name} from {source}"));
                    }
                    for reads in &mut self.reads {
                        reads.insert((name.clone(), source.clone()));
                    }
                }
            }
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
        self.scoped(|this| visit::visit_block(this, block));
    }
}

fn inventory(source: &Source) -> Vec<String> {
    let mut finder = Finder {
        path: &source.path,
        scope: Scope::default(),
        entries: Vec::new(),
        bindings: Vec::new(),
        reads: Vec::new(),
    };
    finder.visit_file(&source.file);
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

mod tests;
