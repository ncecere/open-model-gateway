//! Scoped governance uses the canonical helpers for authority locks and
//! reads authority under those locks without taking additional row locks.
use super::tree::{self, Source, parse_fixture};
use proc_macro2::{TokenStream, TokenTree};
use quote::{ToTokens, quote};
use std::collections::HashSet;
use syn::{
    Expr, LitStr, Token,
    ext::IdentExt,
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
        let quoted = first == '"';
        let lower = word.to_ascii_lowercase();
        let word = if quoted { word } else { lower.as_str() };
        if previous_for && !quoted && word == "share" {
            return Some("FOR SHARE");
        }
        previous_for = !quoted && word == "for";
        if !helper
            && after_trivia(rest).starts_with('(')
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
    module: String,
    function: Option<String>,
    findings: Vec<String>,
    in_block: bool,
    used: HashSet<&'static str>,
}

impl Scan<'_> {
    fn at(&self, item: &'static str) -> bool {
        self.function.as_deref() == Some(item) && !self.used.contains(item)
    }

    fn helper(&self) -> bool {
        self.path == "governance/locks.rs" && self.module == "governance::locks"
    }

    fn check(&mut self, literal: &LitStr) {
        if let Some(rule) = sql_rule(&literal.value(), self.helper()) {
            self.findings.push(format!(
                "{}:{} ({}): {rule}",
                self.path,
                literal.span().start().line,
                self.function.as_deref().unwrap_or(&self.module)
            ));
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    // Only a free function directly declared in this file can own an exception.
    fn visit_file(&mut self, file: &'ast syn::File) {
        for item in &file.items {
            if let syn::Item::Fn(f) = item {
                self.function = Some(format!("{}::{}", self.module, f.sig.ident.unraw()));
                self.visit_item_fn(f);
                self.function = None;
            } else {
                self.visit_item(item);
            }
        }
    }

    fn visit_item(&mut self, item: &'ast syn::Item) {
        let enclosing = self.function.take();
        visit::visit_item(self, item);
        self.function = enclosing;
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let parent = self.in_block;
        self.in_block = true;
        visit::visit_block(self, block);
        self.in_block = parent;
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        if item.ident.unraw() == "sqlx" && item.rename.is_some() {
            self.findings.push(format!(
                "{}:{}: inspect renamed SQLx namespace",
                self.path,
                item.ident.span().start().line
            ));
        }
        visit::visit_item_extern_crate(self, item);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if self.in_block && item.content.is_none() {
            self.findings.push(format!(
                "{}:{}: inspect block-local file module {}",
                self.path,
                item.ident.span().start().line,
                item.ident
            ));
        }
        let parent_len = self.module.len();
        self.module.push_str("::");
        self.module.push_str(&item.ident.unraw().to_string());
        visit::visit_item_mod(self, item);
        self.module.truncate(parent_len);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.check_sqlx_renames(&item.tree, false);
        visit::visit_item_use(self, item);
    }

    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        if !self.helper() && ident.unraw() == "PgAdvisoryLock" {
            self.findings.push(format!(
                "{}:{}: PgAdvisoryLock outside governance::locks",
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
        let suffixes = [
            ("governance::catalog_share", " FOR SHARE OF d,p,m"),
            (
                "governance::reconcile::resolve_usage_once",
                " FOR SHARE OF u,w",
            ),
        ];
        if let Some((item, _)) = suffixes.into_iter().find(|(item, suffix)| {
            self.at(item)
                && branch.to_token_stream().to_string()
                    == quote!(if scoped(store) { "" } else { #suffix }).to_string()
        }) {
            self.used.insert(item);
            return;
        }
        // Job acceptance maps storage errors differently after upstream acceptance.
        const ITEM: &str = "governance::jobs::accept_job";
        if self.at(ITEM)
            && condition.to_token_stream().to_string() == quote!(!scoped(store)).to_string()
            && let Some((first, rest)) = branch.then_branch.stmts.split_first()
            && first.to_token_stream().to_string()
                == quote!(
                    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
                        .execute(&mut *tx).await.map_err(accepted_storage)?;
                )
                .to_string()
        {
            self.used.insert(ITEM);
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
        const ITEM: &str = "governance::catalog_lock";
        if self.at(ITEM)
            && call.to_token_stream().to_string()
                == quote!(sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")).to_string()
        {
            self.used.insert(ITEM);
            return;
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.macro_input(&mac.path.segments.last().unwrap().ident, mac.tokens.clone());
    }
}

impl Scan<'_> {
    fn check_sqlx_renames(&mut self, tree: &syn::UseTree, sqlx: bool) {
        match tree {
            syn::UseTree::Path(path) if !sqlx => match path.ident.unraw().to_string().as_str() {
                "sqlx" => self.check_sqlx_renames(&path.tree, true),
                "crate" | "super" | "self" => self.check_sqlx_renames(&path.tree, false),
                _ => {}
            },
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.check_sqlx_renames(item, sqlx);
                }
            }
            syn::UseTree::Rename(rename)
                if rename.ident.unraw() == "sqlx" || (sqlx && rename.ident.unraw() == "self") =>
            {
                self.findings.push(format!(
                    "{}:{}: inspect renamed SQLx namespace",
                    self.path,
                    rename.ident.span().start().line
                ));
            }
            syn::UseTree::Rename(rename) if sqlx => {
                let name = rename.ident.unraw().to_string();
                let core = name.strip_suffix("_unchecked").unwrap_or(&name);
                if matches!(
                    core,
                    "query"
                        | "query_as"
                        | "query_scalar"
                        | "query_file"
                        | "query_file_as"
                        | "query_file_scalar"
                ) {
                    self.findings.push(format!(
                        "{}:{}: inspect renamed SQLx query {}",
                        self.path,
                        rename.ident.span().start().line,
                        rename.ident
                    ));
                }
            }
            _ => {}
        }
    }

    fn macro_input(&mut self, name: &syn::Ident, tokens: TokenStream) {
        let name = name.unraw();
        if name == "concat"
            && let Ok(parts) =
                Punctuated::<LitStr, Token![,]>::parse_terminated.parse2(tokens.clone())
        {
            let sql: String = parts.iter().map(LitStr::value).collect();
            self.check(&LitStr::new(&sql, name.span()));
            return;
        }
        if name == "include_str" || name == "concat" || name.to_string().starts_with("query_file") {
            self.findings.push(format!(
                "{} ({}): inspect {name}! SQL input",
                self.path,
                self.function.as_deref().unwrap_or(&self.module)
            ));
            return;
        }
        let name_str = name.to_string();
        let query = name_str.strip_suffix("_unchecked").unwrap_or(&name_str);
        if matches!(query, "query" | "query_as" | "query_scalar") {
            let parse = |input: syn::parse::ParseStream<'_>| {
                let record = if query == "query_as" {
                    let path = input.parse::<syn::Path>()?;
                    input.parse::<Token![,]>()?;
                    Some(path)
                } else {
                    None
                };
                let parts = Punctuated::<LitStr, Token![+]>::parse_separated_nonempty(input)?;
                if !input.is_empty() {
                    input.parse::<Token![,]>()?;
                }
                Ok((record, parts, input.parse::<TokenStream>()?))
            };
            if let Ok((record, parts, args)) = parse.parse2(tokens.clone()) {
                if let Some(path) = record {
                    self.visit_path(&path);
                }
                let sql: String = parts.iter().map(LitStr::value).collect();
                self.check(&LitStr::new(&sql, name.span()));
                self.macro_args(args);
            } else {
                self.findings
                    .push(format!("{}: inspect {name}! SQL input", self.path));
            }
            return;
        }
        self.macro_args(tokens);
    }

    fn macro_args(&mut self, tokens: TokenStream) {
        if let Ok(args) = Punctuated::<Expr, Token![,]>::parse_terminated.parse2(tokens.clone()) {
            for arg in args {
                self.visit_expr(&arg);
            }
        } else {
            self.opaque_tokens(tokens);
        }
    }

    fn opaque_tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        let mut rest = tokens.as_slice();
        while let Some((first, tail)) = rest.split_first() {
            if let [
                TokenTree::Ident(name),
                TokenTree::Punct(bang),
                TokenTree::Group(args),
                more @ ..,
            ] = rest
                && bang.as_char() == '!'
            {
                self.visit_ident(name);
                self.macro_input(name, args.stream());
                rest = more;
                continue;
            }
            match first {
                TokenTree::Group(group) => self.opaque_tokens(group.stream()),
                TokenTree::Ident(ident) => self.visit_ident(ident),
                TokenTree::Literal(lit) => {
                    if let Ok(value) = syn::parse2::<LitStr>(lit.to_token_stream()) {
                        self.check(&value);
                    }
                }
                TokenTree::Punct(_) => {}
            }
            rest = tail;
        }
    }
}

fn violations(source: &Source) -> Vec<String> {
    let mut scan = Scan {
        path: &source.path,
        module: source.module.clone(),
        function: None,
        findings: Vec::new(),
        in_block: false,
        used: HashSet::new(),
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
            "governance/locks.rs",
            r#"mod nested { fn f() { q!("select omg_lock_scopes($1,$2,$3)"); } }"#,
            1,
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

#[test]
fn catalog_exception_belongs_to_one_root_item_per_file() {
    let root = r#"fn catalog_share() { if scoped(store) { "" } else { " FOR SHARE OF d,p,m" } }"#;
    for (rust, expected) in [
        (root.to_owned(), 0),
        (format!("mod inner {{ {root} }}"), 1),
        (format!("{root} mod inner {{ {root} }}"), 1),
        (format!("#[cfg(a)] {root} #[cfg(b)] {root}"), 1),
    ] {
        assert_eq!(
            violations(&parse_fixture("governance.rs", &rust)).len(),
            expected,
            "{rust}"
        );
    }
}

#[test]
fn path_attribute_does_not_grant_module_identity() {
    let read = |path: &std::path::Path| {
        Some(
            match path.to_str()? {
                "governance.rs" => r#"#[path = "governance/jobs.rs"] mod other;"#,
                "governance/jobs.rs" => {
                    r#"fn accept_job() {
            if !scoped(store) {
                sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
                    .execute(&mut *tx).await.map_err(accepted_storage)?;
            }
        }"#
                }
                _ => return None,
            }
            .to_owned(),
        )
    };
    let files = tree::load_tree(&read, "governance.rs", false);
    assert_eq!(files.iter().flat_map(violations).count(), 1);
}

#[test]
fn opaque_macro_inputs_keep_nested_invocations() {
    for (input, expected) in [
        (r#"sqlx::query_file!("example.sql")"#, 1),
        (r#"include_str!("example.sql")"#, 1),
        (r#"concat!("FOR ", "SHARE")"#, 1),
        (r#"concat!("SELECT ", "1")"#, 0),
        (r#"concat!("SELECT ", value)"#, 1),
    ] {
        let rust = format!("fn example() {{ wrapper!(key => [nested!(key => {input})]); }}");
        assert_eq!(
            violations(&parse_fixture("governance/admission.rs", &rust)).len(),
            expected,
            "{input}"
        );
    }
}

#[test]
fn quoted_identifiers_and_function_references() {
    for (sql, expected) in [
        (r#"SELECT "for" "share""#, None),
        ("SELECT pg_advisory_lock FROM examples", None),
        (r#"SELECT "PG_ADVISORY_LOCK"(1)"#, None),
        (
            r#"SELECT "pg_advisory_lock" /* comment */ (1)"#,
            Some("direct lock acquisition"),
        ),
        (
            "SELECT PG_ADVISORY_LOCK (1)",
            Some("direct lock acquisition"),
        ),
    ] {
        assert_eq!(sql_rule(sql, false), expected, "{sql}");
    }
}

#[test]
fn sqlx_query_literal_addition() {
    for (query, expected) in [
        (
            r#"sqlx::query!("SELECT 1 FOR " + "SHARE")"#,
            Some("FOR SHARE"),
        ),
        (
            r#"sqlx::query_as!(Row, "SELECT pg_advisory_" + "lock(1)")"#,
            Some("direct lock acquisition"),
        ),
        (r#"sqlx::query_scalar!("SELECT " + "1", n + 1)"#, None),
    ] {
        for expr in [query.to_owned(), format!("opaque!(key => [{query}])")] {
            let rust = format!("fn f() {{ {expr}; }}");
            let findings = violations(&parse_fixture("governance/jobs.rs", &rust));
            assert_eq!(
                findings.len(),
                usize::from(expected.is_some()),
                "{expr}: {findings:?}"
            );
            if let Some(rule) = expected {
                assert!(findings[0].ends_with(rule), "{expr}: {findings:?}");
            }
        }
    }
}

#[test]
fn block_local_locks_module_does_not_own_the_helper_exception() {
    let rust = r#"fn foo() { mod locks { fn bar() { q!("SELECT pg_advisory_lock(1)"); } } }"#;
    let findings = violations(&parse_fixture("governance.rs", rust));
    assert!(
        findings
            .iter()
            .any(|finding| finding.ends_with("direct lock acquisition"))
    );
}

#[test]
fn explicit_sqlx_macro_renames_require_inspection() {
    for (rust, expected) in [
        (
            r#"use sqlx::query as q; fn f() { q!("SELECT 1 FOR " + "SHARE"); }"#,
            "governance/jobs.rs:1: inspect renamed SQLx query query",
        ),
        (
            r#"use sqlx::{query_file as q}; fn f() { q!("fixture.sql"); }"#,
            "governance/jobs.rs:1: inspect renamed SQLx query query_file",
        ),
        (
            "use sqlx; mod child {\nuse super::sqlx::query_file as load; }",
            "governance/jobs.rs:2: inspect renamed SQLx query query_file",
        ),
        (
            "mod sqlx { pub fn query() {} }\nuse self::sqlx::query as local_query;",
            "governance/jobs.rs:2: inspect renamed SQLx query query",
        ),
        (
            "use sqlx; mod child {\nuse crate::sqlx::query_file as load; }",
            "governance/jobs.rs:2: inspect renamed SQLx query query_file",
        ),
        (
            "use sqlx::{postgres::{self as pg}, query_file as load};",
            "governance/jobs.rs:1: inspect renamed SQLx query query_file",
        ),
    ] {
        assert_eq!(
            violations(&parse_fixture("governance/jobs.rs", rust)),
            [expected],
            "{rust}"
        );
    }
    assert!(
        violations(&parse_fixture(
            "governance/jobs.rs",
            "use application::query as q;"
        ))
        .is_empty()
    );
}

#[test]
fn renamed_sqlx_namespace_and_file_modules_in_blocks_require_inspection() {
    for source in [
        "use sqlx::postgres::{self as pg};",
        "use app::sqlx as local;",
    ] {
        assert!(
            violations(&parse_fixture("governance.rs", source)).is_empty(),
            "{source}"
        );
    }

    for (source, rule) in [
        ("use sqlx as db; use db::query_file as q;", "SQLx namespace"),
        ("use sqlx::{self as db};", "SQLx namespace"),
        ("extern crate sqlx as db;", "SQLx namespace"),
        (
            r#"fn owner() { #[path = "child.rs"] mod locks; }"#,
            "block-local file module",
        ),
    ] {
        let findings = violations(&parse_fixture("governance.rs", source));
        assert!(
            findings.iter().any(|finding| finding.contains(rule)),
            "{source}: {findings:?}"
        );
    }
}
