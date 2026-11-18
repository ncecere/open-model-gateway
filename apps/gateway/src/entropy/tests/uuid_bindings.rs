//! Direct value bindings in production syntax. This is a naming check, without type or data-flow inference.
mod sql;

use proc_macro2::{TokenStream, TokenTree};
use std::path::Path;
use syn::ext::IdentExt;
use syn::visit::{self, Visit};
use syn::{Expr, Member, Pat};

fn sensitive(name: &str) -> bool {
    // key_id names the public row identifier paired with independently generated secret material.
    let lower = name.to_ascii_lowercase();
    name != "key_id"
        && ["secret", "token", "nonce", "key", "salt", "verifier"]
            .iter()
            .any(|part| lower.contains(part))
}

fn uuid_value(expr: &Expr) -> bool {
    match expr {
        Expr::MethodCall(e) => uuid_value(&e.receiver),
        Expr::Paren(e) => uuid_value(&e.expr),
        Expr::Reference(e) => uuid_value(&e.expr),
        Expr::Call(e) => {
            let constructor = matches!(&*e.func, Expr::Path(p) if {
                let mut path = p.path.segments.iter().rev();
                path.next().is_some_and(|s| matches!(s.ident.to_string().as_str(), "new_v4" | "new_v7" | "now_v7"))
                    && path.next().is_some_and(|s| s.ident == "Uuid")
            });
            constructor || e.args.iter().any(uuid_value)
        }
        _ => false,
    }
}

struct Bindings<'a> {
    path: &'a str,
    function: String,
    problems: Vec<String>,
}

impl Bindings<'_> {
    fn binding(&mut self, ident: &syn::Ident, expr: &Expr, local: bool) {
        let name = ident.unraw().to_string();
        if !sensitive(&name) || !uuid_value(expr) {
            return;
        }
        // The report run's claim_token rejects stale generation outcomes. The two
        // outbox workers compare lease_token with the lease owner before settling delivery.
        if local
            && name == "token"
            && matches!(
                (self.path, self.function.as_str()),
                ("scheduled_reports.rs", "claim_with_clock")
                    | ("alerts/outbox.rs", "claim_with_origin")
                    | ("alerts/webhooks.rs", "claim")
            )
        {
            return;
        }
        self.problems.push(format!(
            "{}:{}: UUID bound to {name}",
            self.path,
            ident.span().start().line
        ));
    }

    fn pattern(&mut self, pat: &Pat, expr: &Expr) {
        match (pat, expr) {
            (Pat::Ident(p), _) => self.binding(&p.ident, expr, true),
            (Pat::Type(p), _) => self.pattern(&p.pat, expr),
            (Pat::Tuple(p), Expr::Tuple(values)) => {
                for (p, value) in p.elems.iter().zip(&values.elems) {
                    self.pattern(p, value);
                }
            }
            _ => {}
        }
    }

    fn macro_literals(&mut self, tokens: TokenStream) {
        for token in tokens {
            match token {
                TokenTree::Group(g) => self.macro_literals(g.stream()),
                TokenTree::Literal(l) => {
                    sql::scan(self.path, &super::literal_text(&l), &mut self.problems)
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Bindings<'_> {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if !super::test_only(&m.attrs) {
            visit::visit_item_mod(self, m);
        }
    }
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        let prev = std::mem::replace(&mut self.function, f.sig.ident.to_string());
        visit::visit_item_fn(self, f);
        self.function = prev;
    }
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Some(init) = &local.init {
            self.pattern(&local.pat, &init.expr);
        }
        visit::visit_local(self, local);
    }
    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        if let Member::Named(id) = &field.member {
            self.binding(id, &field.expr, false);
        }
        visit::visit_field_value(self, field);
    }
    fn visit_expr_assign(&mut self, a: &'ast syn::ExprAssign) {
        if let Expr::Field(f) = &*a.left
            && let Member::Named(id) = &f.member
        {
            self.binding(id, &a.right, false);
        }
        visit::visit_expr_assign(self, a);
    }
    fn visit_lit_str(&mut self, s: &'ast syn::LitStr) {
        sql::scan(self.path, &s.value(), &mut self.problems);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if m.path.segments.last().is_some_and(|s| s.ident == "concat") {
            sql::scan(
                self.path,
                &super::concat_text(m.tokens.clone()),
                &mut self.problems,
            );
        }
        self.macro_literals(m.tokens.clone());
    }
}

pub(super) fn rust(path: &str, ast: &syn::File) -> Vec<String> {
    let mut scan = Bindings {
        path,
        function: String::new(),
        problems: Vec::new(),
    };
    scan.visit_file(ast);
    scan.problems
}

pub(super) fn sql_files() -> Vec<String> {
    fn walk(root: &Path, dir: &Path, problems: &mut Vec<String>) {
        for entry in dir.read_dir().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, problems);
            } else if path.extension().is_some_and(|ext| ext == "sql") {
                sql::scan(
                    path.strip_prefix(root).unwrap().to_str().unwrap(),
                    &std::fs::read_to_string(&path).unwrap(),
                    problems,
                );
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let mut problems = Vec::new();
    for dir in [
        "apps/gateway/src",
        "apps/gateway/migrations",
        "apps/gateway/enterprise_migrations",
        "deploy/staging",
    ] {
        walk(root, &root.join(dir), &mut problems);
    }
    problems
}

#[test]
fn identifier_and_fence_exceptions_have_narrow_boundaries() {
    for (path, func) in [
        ("scheduled_reports.rs", "claim_with_clock"),
        ("alerts/outbox.rs", "claim_with_origin"),
        ("alerts/webhooks.rs", "claim"),
    ] {
        let ast =
            syn::parse_file(&format!("fn {func}() {{ let token = Uuid::new_v4(); }}")).unwrap();
        assert!(rust(path, &ast).is_empty());
        assert_eq!(rust("other.rs", &ast).len(), 1);
        let ast = syn::parse_file(&format!("fn {func}() {{ Item {{ token: Uuid::now_v7() }}; }} fn unrelated() {{ let token = Uuid::new_v4(); }}")).unwrap();
        assert_eq!(rust(path, &ast).len(), 2);
    }

    let ast = syn::parse_file(
        "fn f() { let (key_id, session_key_id) = (Uuid::new_v4(), Uuid::new_v4()); }",
    )
    .unwrap();
    let problems = rust("fixture.rs", &ast);
    assert_eq!(problems.len(), 1);
    assert!(problems[0].ends_with("session_key_id"));
}

#[test]
fn sql_values_are_matched_to_their_target_columns() {
    for (text, expected) in [
        (
            "INSERT INTO t(id, salt) VALUES (gen_random_uuid(), 'fixed'), (coalesce($1,$2), GEN_RANDOM_UUID ())",
            &["salt"][..],
        ),
        (
            "INSERT INTO schema.t(key_id, verifier) SELECT gen_random_uuid(), (gen_random_uuid())::text FROM src",
            &["verifier"],
        ),
        (
            "ALTER TABLE t ALTER COLUMN \"SessionToken\" SET DEFAULT pg_catalog.gen_random_uuid()",
            &["sessiontoken"],
        ),
        (
            "UPDATE t SET secret=coalesce($1, gen_random_uuid()) WHERE token=gen_random_uuid()",
            &["secret"],
        ),
        (
            "SELECT gen_random_uuid() AS client_nonce, gen_random_uuid() AS id",
            &["client_nonce"],
        ),
        (
            "CREATE TABLE token_records(id uuid PRIMARY KEY DEFAULT gen_random_uuid(), salt text)",
            &[],
        ),
        (
            "UPDATE t SET salt='gen_random_uuid()' /* token=gen_random_uuid() */",
            &[],
        ),
        (
            "INSERT INTO report_runs(claim_token) VALUES(gen_random_uuid())",
            &[],
        ),
        (
            "INSERT INTO other(claim_token) VALUES(gen_random_uuid())",
            &["claim_token"],
        ),
        (
            "UPDATE other SET lease_token=gen_random_uuid()",
            &["lease_token"],
        ),
        (
            "UPDATE alert_webhook_outbox SET lease_token=gen_random_uuid()",
            &[],
        ),
    ] {
        let mut problems = Vec::new();
        sql::scan("deploy/staging/verify-privileges.sql", text, &mut problems);
        let names: Vec<_> = problems
            .iter()
            .map(|p| p.split_whitespace().last().unwrap())
            .collect();
        assert_eq!(names, expected, "{text}");
    }
}
