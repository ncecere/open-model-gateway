//! Scoped governance uses the canonical helpers for authority locks and
//! reads authority under those locks without taking additional row locks.
use super::tree::{self, Source, parse_fixture};
use proc_macro2::TokenTree;
use quote::{ToTokens, quote};
use syn::{
    Expr, LitStr, Token,
    parse::Parser,
    punctuated::Punctuated,
    visit::{self, Visit},
};

fn after_trivia(mut rest: &str) -> &str {
    loop {
        rest = rest.trim_start();
        if let Some(comment) = rest.strip_prefix("--") {
            rest = comment.find(['\n', '\r']).map_or("", |i| &comment[i..]);
        } else if let Some(mut comment) = rest.strip_prefix("/*") {
            let mut depth = 1;
            while depth > 0 {
                let Some(i) = comment.find(['*', '/']) else {
                    return "";
                };
                comment = &comment[i..];
                if let Some(inner) = comment.strip_prefix("/*") {
                    depth += 1;
                    comment = inner;
                } else if let Some(tail) = comment.strip_prefix("*/") {
                    depth -= 1;
                    comment = tail;
                } else {
                    comment = &comment[1..];
                }
            }
            rest = comment;
        } else {
            return rest;
        }
    }
}

// Starting at the opening delimiter, consume a PostgreSQL quoted token.
// E strings additionally allow a backslash to escape the following character.
fn quoted_end(input: &str, quote: char, escapes: bool) -> usize {
    let mut chars = input.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        if escapes && c == '\\' {
            chars.next();
        } else if c == quote && chars.next_if(|(_, next)| *next == quote).is_none() {
            return i + 1;
        }
    }
    input.len()
}

fn dollar_tag(input: &str) -> Option<&str> {
    let tail = input.strip_prefix('$')?;
    let (tag, _) = tail.split_once('$')?;
    if tag.starts_with(|c: char| c.is_ascii_digit())
        || !tag.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(&input[..tag.len() + 2])
}

fn sql_rule(sql: &str, helper: bool) -> Option<&'static str> {
    // Keep original case while finding the matching dollar-quote delimiter.
    let separator = |c: char| !c.is_alphanumeric() && c != '_' && c != '$';
    let mut rest = sql;
    let mut previous_for = false;
    loop {
        rest = after_trivia(rest);
        let Some(first) = rest.chars().next() else {
            break;
        };
        let escaped = rest.starts_with("E'") || rest.starts_with("e'");
        let word = if escaped || first == '\'' || first == '"' {
            if escaped {
                rest = &rest[1..];
            }
            let delimiter = if escaped { '\'' } else { first };
            let end = quoted_end(rest, delimiter, escaped);
            let token = if first == '"' {
                rest[1..end].strip_suffix('"').unwrap_or("")
            } else {
                ""
            };
            rest = &rest[end..];
            token
        } else if let Some(tag) = dollar_tag(rest) {
            let body = &rest[tag.len()..];
            rest = body.split_once(tag).map_or("", |(_, tail)| tail);
            ""
        } else if separator(first) {
            rest = &rest[first.len_utf8()..];
            ""
        } else {
            let end = rest.find(separator).unwrap_or(rest.len());
            let token = &rest[..end];
            rest = &rest[end..];
            token
        };
        let lower = word.to_ascii_lowercase();
        let word = lower.as_str();
        if previous_for && word == "share" {
            return Some("FOR SHARE");
        }
        previous_for = word == "for";
        if !helper
            && matches!(
                word,
                "pg_advisory_lock"
                    | "pg_advisory_lock_shared"
                    | "pg_try_advisory_lock"
                    | "pg_try_advisory_lock_shared"
                    | "pg_advisory_xact_lock"
                    | "pg_advisory_xact_lock_shared"
                    | "pg_try_advisory_xact_lock"
                    | "pg_try_advisory_xact_lock_shared"
                    | "omg_admission_locks"
                    | "omg_scope_lock_exclusive"
                    | "omg_lock_scopes"
                    | "omg_lock_scope_rows"
            )
        {
            return Some("direct lock acquisition");
        }
    }
    None
}

struct Scan<'a> {
    path: &'a str,
    function: String,
    findings: Vec<String>,
    exception_used: bool,
}

impl Scan<'_> {
    fn at(&self, path: &str, function: &str) -> bool {
        self.path == path && self.function == function
    }

    fn check(&mut self, literal: &LitStr) {
        if let Some(rule) = sql_rule(&literal.value(), self.path == "governance/locks.rs") {
            self.findings.push(format!(
                "{}:{} ({}): {rule}",
                self.path,
                literal.span().start().line,
                self.function
            ));
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        let previous = std::mem::replace(&mut self.function, f.sig.ident.to_string());
        let used = std::mem::take(&mut self.exception_used);
        visit::visit_item_fn(self, f);
        self.exception_used = used;
        self.function = previous;
    }

    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        use syn::ext::IdentExt;
        if self.path != "governance/locks.rs" && ident.unraw() == "PgAdvisoryLock" {
            self.findings.push(format!(
                "{}:{}: PgAdvisoryLock outside governance/locks.rs",
                self.path,
                ident.span().start().line
            ));
        }
    }

    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

    fn visit_lit_str(&mut self, literal: &'ast LitStr) {
        self.check(literal);
    }

    fn visit_expr_if(&mut self, branch: &'ast syn::ExprIf) {
        let condition = &branch.cond;
        // Each suffix belongs exclusively to the global-mode arm.
        let suffix = if self.at("governance.rs", "catalog_share") {
            " FOR SHARE OF d,p,m"
        } else if self.at("governance/reconcile.rs", "resolve_usage_once") {
            " FOR SHARE OF u,w"
        } else {
            ""
        };
        if !suffix.is_empty()
            && !self.exception_used
            && branch.to_token_stream().to_string()
                == quote!(if scoped(store) { "" } else { #suffix }).to_string()
        {
            self.exception_used = true;
            return;
        }
        // Job acceptance maps storage errors differently after upstream acceptance.
        if self.at("governance/jobs.rs", "accept_job")
            && !self.exception_used
            && condition.to_token_stream().to_string() == quote!(!scoped(store)).to_string()
            && let Some((first, rest)) = branch.then_branch.stmts.split_first()
            && first.to_token_stream().to_string()
                == quote!(
                    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
                        .execute(&mut *tx).await.map_err(accepted_storage)?;
                )
                .to_string()
        {
            self.exception_used = true;
            for stmt in rest {
                self.visit_stmt(stmt);
            }
            if let Some((_, other)) = &branch.else_branch {
                self.visit_expr(other);
            }
            return;
        }
        visit::visit_expr_if(self, branch);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        // The singleton catalog fence uses the separate one-bigint key space.
        if self.at("governance.rs", "catalog_lock")
            && !self.exception_used
            && call.to_token_stream().to_string()
                == quote!(sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")).to_string()
        {
            self.exception_used = true;
            return;
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = &mac.path.segments.last().unwrap().ident;
        if name == "concat"
            && let Ok(parts) =
                Punctuated::<LitStr, Token![,]>::parse_terminated.parse2(mac.tokens.clone())
        {
            let sql: String = parts.iter().map(LitStr::value).collect();
            self.check(&LitStr::new(&sql, name.span()));
            return;
        }
        if name == "include_str" || name == "concat" || name.to_string().starts_with("query_file") {
            self.findings.push(format!(
                "{} ({}): inspect {name}! SQL input",
                self.path, self.function
            ));
            return;
        }
        if let Ok(args) = Punctuated::<Expr, Token![,]>::parse_terminated.parse2(mac.tokens.clone())
        {
            for arg in args {
                self.visit_expr(&arg);
            }
        } else {
            for token in tree::flat(mac.tokens.clone()) {
                if let TokenTree::Ident(ident) = &token {
                    self.visit_ident(ident);
                }
                if let TokenTree::Literal(lit) = token
                    && let Ok(value) = syn::parse2::<LitStr>(lit.to_token_stream())
                {
                    self.check(&value);
                }
            }
        }
    }
}

fn violations(source: &Source) -> Vec<String> {
    let mut scan = Scan {
        path: &source.path,
        function: String::new(),
        findings: Vec::new(),
        exception_used: false,
    };
    scan.visit_file(&source.file);
    scan.findings
}

#[test]
fn production_governance_lock_sql() {
    let files = tree::production("governance.rs");
    for required in [
        "governance/admission.rs",
        "governance/settlement.rs",
        "governance/locks.rs",
    ] {
        assert!(
            files.iter().any(|file| file.path == required),
            "missing {required}"
        );
    }
    let problems: Vec<_> = files.iter().flat_map(violations).collect();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
#[test]
fn planted_lock_sql_is_refused() {
    for sql in [
        "SELECT * FROM users u FoR /* row */ SHARE OF u",
        "SELECT pg_catalog.pg_try_advisory_xact_lock_shared(1,2)",
        "SELECT omg_scope_lock_exclusive(1,2)",
    ] {
        let source = format!("fn example() {{ sqlx::query({sql:?}); }}");
        let fixture = parse_fixture("governance/admission.rs", &source);
        assert_eq!(violations(&fixture).len(), 1, "missed {sql}");
    }
}

#[test]
fn macros_helpers_and_global_branches() {
    let cases = [
        (
            "governance/settlement.rs",
            "use sqlx::postgres::PgAdvisoryLock as Lock;",
            1,
        ),
        (
            "governance/settlement.rs",
            r##"fn f() { q!(r#"select * from workspaces for -- split
 SHARE"#); }"##,
            1,
        ),
        (
            "governance/settlement.rs",
            r#"fn f() { q!(concat!("select pg_advisory_", "lock(1)")); }"#,
            1,
        ),
        (
            "governance/settlement.rs",
            r#"fn f() { q!("select 'FOR SHARE', 1 /* pg_advisory_lock(1) */ FOR UPDATE"); }"#,
            0,
        ),
        (
            "governance/settlement.rs",
            r#"fn f() { q!("SELECT $$FOR SHARE$$, 'pg_advisory_lock'"); }"#,
            0,
        ),
        (
            "governance/settlement.rs",
            r#"fn f() { q!("SELECT $Tag$' /* $Tag$ FROM users FOR SHARE"); }"#,
            1,
        ),
        (
            "governance/settlement.rs",
            r##"fn f() { q!(r#"SELECT pg_catalog."pg_advisory_lock"(1)"#); }"##,
            1,
        ),
        (
            "governance/locks.rs",
            r#"fn f() { q!("select omg_lock_scopes($1,$2,$3)"); }"#,
            0,
        ),
        (
            "governance.rs",
            r#"fn catalog_share() { if !scoped(store) { "" } else { " FOR SHARE OF d,p,m" } }"#,
            1,
        ),
        (
            "governance/reconcile.rs",
            r#"fn resolve_usage_once() { format!("{}", if scoped(store) { "" } else { " FOR SHARE OF u,w" }); q!(" FOR SHARE OF u,w"); }"#,
            1,
        ),
        (
            "governance/jobs.rs",
            r#"fn accept_job() { sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)"); }"#,
            1,
        ),
        (
            "governance.rs",
            r#"fn catalog_lock() { sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)"); sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)"); }"#,
            1,
        ),
        (
            "governance.rs",
            r#"#[cfg(test)] fn f() { q!("SELECT pg_advisory_lock(1)"); }"#,
            0,
        ),
    ];
    for (path, rust, expected) in cases {
        let problems = violations(&parse_fixture(path, rust));
        assert_eq!(problems.len(), expected, "{rust}: {problems:?}");
    }
}
