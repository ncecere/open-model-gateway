//! Mechanical inventory of production route registrations, independent of the policy table.
use std::{collections::BTreeSet, path::Path};
use syn::{
    Expr, Lit,
    visit::{self, Visit},
};

type Inventory = BTreeSet<(String, String)>;

// Evaluate only what is known in a production build (test=false). Unknown
// feature/target predicates are included, so optional production routes count.
fn production_cfg(meta: &syn::Meta) -> Option<bool> {
    match meta {
        syn::Meta::Path(p) if p.is_ident("test") => Some(false),
        syn::Meta::List(list) => {
            let args = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .unwrap();
            let values: Vec<_> = args.iter().map(production_cfg).collect();
            if list.path.is_ident("not") && values.len() == 1 {
                values[0].map(|v| !v)
            } else if list.path.is_ident("all") {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|v| *v == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if list.path.is_ident("any") {
                if values.contains(&Some(true)) {
                    Some(true)
                } else if values.iter().all(|v| *v == Some(false)) {
                    Some(false)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}
fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.parse_args::<syn::Meta>()
                .is_ok_and(|m| production_cfg(&m) == Some(false))
    })
}

#[derive(Default)]
struct Scanner {
    prefix: String,
    routes: Inventory,
}
fn literal(expr: &Expr) -> String {
    match expr {
        Expr::Lit(l) => match &l.lit {
            Lit::Str(s) => s.value(),
            _ => panic!("route path must be a string"),
        },
        // The public logo is shared with its response metadata.
        Expr::Path(p) if p.path.is_ident("LOGO_PATH") => {
            crate::management::branding::LOGO_PATH.to_owned()
        }
        _ => {
            panic!("unresolved route path: declare a literal or teach the inventory to resolve it")
        }
    }
}
#[derive(Default)]
struct Methods(BTreeSet<String>);
impl Methods {
    fn add(&mut self, name: &str) {
        match name {
            "get" | "post" | "put" | "patch" | "delete" | "head" | "options" | "trace"
            | "connect" => {
                self.0.insert(name.to_uppercase());
            }
            "any" => {
                self.0.insert("*".to_owned());
            }
            "on" | "on_service" | "any_service" => panic!("unresolved method registration"),
            _ => (),
        }
    }
}
impl<'ast> Visit<'ast> for Methods {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let Expr::Path(p) = &*node.func {
            self.add(&p.path.segments.last().unwrap().ident.to_string());
        }
        visit::visit_expr_call(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.add(&node.method.to_string());
        visit::visit_expr_method_call(self, node);
    }
}
impl<'ast> Visit<'ast> for Scanner {
    fn visit_item(&mut self, node: &'ast syn::Item) {
        let attrs = match node {
            syn::Item::Mod(i) => &i.attrs,
            syn::Item::Fn(i) => &i.attrs,
            syn::Item::Impl(i) => &i.attrs,
            _ => {
                visit::visit_item(self, node);
                return;
            }
        };
        if !test_only(attrs) {
            visit::visit_item(self, node);
        }
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let tokens = node.tokens.to_string();
        assert!(
            !tokens.contains(". route (")
                && !tokens.contains(". nest (")
                && !tokens.contains(". route_service (")
                && !tokens.contains(". nest_service ("),
            "router registrations inside macros need inventory support"
        );
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "nest" {
            self.visit_expr(&node.receiver);
            let prefix = literal(&node.args[0]);
            let old = self.prefix.clone();
            self.prefix.push_str(prefix.trim_end_matches('/'));
            // Inline nesting is resolved. Named routers must not silently lose the prefix.
            assert!(
                !matches!(node.args[1], Expr::Call(_)),
                "resolve named nested router in inventory"
            );
            let count = self.routes.len();
            self.visit_expr(&node.args[1]);
            assert!(
                self.routes.len() > count,
                "unresolved nested router in inventory"
            );
            self.prefix = old;
            return;
        }
        assert!(
            node.method != "route_service" && node.method != "nest_service",
            "service routes need an explicit inventory implementation"
        );
        if node.method == "route" {
            let path = format!("{}{}", self.prefix, literal(&node.args[0]));
            let mut methods = Methods::default();
            methods.visit_expr(&node.args[1]);
            assert!(!methods.0.is_empty(), "unresolved method router for {path}");
            for method in methods.0 {
                self.routes.insert((method, path.clone()));
            }
        }
        visit::visit_expr_method_call(self, node);
    }
}
fn scan(dir: &Path, scanner: &mut Scanner) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan(&path, scanner);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let name = path.file_name().unwrap().to_str().unwrap();
            if name == "tests.rs" || name.ends_with("_tests.rs") || name == "authz_matrix.rs" {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            scanner.visit_file(&syn::parse_file(&source).unwrap());
        }
    }
}
pub(crate) fn inventory() -> Inventory {
    let mut scanner = Scanner::default();
    scan(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut scanner,
    );
    scanner.routes
}

fn inventory_difference(actual: &Inventory, declared: &Inventory) -> String {
    let missing: Vec<_> = actual.difference(declared).collect();
    let extra: Vec<_> = declared.difference(actual).collect();
    format!(
        "Missing declarations (method, template): {missing:#?}\nExtra declarations (method, template): {extra:#?}\nFor each route, declare it in apps/gateway/src/route_policy.rs"
    )
}
#[test]
fn inventory_failure_names_missing_extra_and_contributor_action() {
    let actual = [("GET".to_owned(), "/new".to_owned())]
        .into_iter()
        .collect();
    let declared = [("POST".to_owned(), "/removed".to_owned())]
        .into_iter()
        .collect();
    let message = inventory_difference(&actual, &declared);
    for text in [
        "Missing declarations",
        "Extra declarations",
        "GET",
        "/new",
        "POST",
        "/removed",
        "declare it in apps/gateway/src/route_policy.rs",
    ] {
        assert!(message.contains(text), "{message}");
    }
}
#[test]
fn every_served_route_has_an_exact_authorization_declaration() {
    let actual = inventory();
    let declared: Inventory = crate::route_policy::ROUTES
        .iter()
        .flat_map(|r| {
            r.methods
                .split_whitespace()
                .map(move |m| (m.to_owned(), r.path.to_owned()))
        })
        .collect();
    assert!(
        actual == declared,
        "{}",
        inventory_difference(&actual, &declared)
    );
    let count: usize = crate::route_policy::ROUTES
        .iter()
        .map(|r| r.methods.split_whitespace().count())
        .sum();
    assert_eq!(
        count,
        declared.len(),
        "duplicate authorization declarations; {}",
        inventory_difference(&actual, &declared)
    );
}
#[test]
fn removed_installation_policy_requires_platform_authority_per_method() {
    use crate::route_policy::{Access, lookup};
    assert_eq!(
        lookup("GET", "/api/v1/platform/installation/policy"),
        Some(Access::PlatformRead)
    );
    assert_eq!(
        lookup("PUT", "/api/v1/platform/installation/policy"),
        Some(Access::PlatformWrite)
    );
}
#[test]
fn unauthenticated_surface_is_explicitly_pinned() {
    use crate::route_policy::{Access, ROUTES};
    let actual: Inventory = ROUTES
        .iter()
        .filter(|r| matches!(r.access, Access::Anonymous | Access::Health))
        .flat_map(|r| {
            r.methods
                .split_whitespace()
                .map(move |m| (m.to_owned(), r.path.to_owned()))
        })
        .collect();
    let expected: Inventory = [
        ("GET", "/api/v1/auth/config"),
        ("GET", "/api/v1/auth/login"),
        ("GET", "/api/v1/auth/callback"),
        ("GET", "/api/v1/branding/logo"),
        ("GET", "/metrics"),
        ("GET", "/health/live"),
        ("GET", "/health/ready"),
    ]
    .map(|(m, p)| (m.to_owned(), p.to_owned()))
    .into_iter()
    .collect();
    assert_eq!(
        actual, expected,
        "changing the public surface requires an explicit security review and allowlist update"
    );
}
#[test]
fn authorization_lookup_rejects_prefix_and_method_lookalikes() {
    use crate::route_policy::{Access, lookup};
    assert_eq!(
        lookup("GET", "/api/v1/platform/providers"),
        Some(Access::PlatformRead)
    );
    assert_eq!(
        lookup("POST", "/api/v1/platform/providers"),
        Some(Access::PlatformWrite)
    );
    assert_eq!(lookup("GET", "/api/v1/platform/providers-extra"), None);
    assert_eq!(lookup("GET", "/api/v1/platform/providers/anything"), None);
    assert_eq!(lookup("POST", "/api/v1/platform/server-policy"), None);
    assert_eq!(lookup("POST", "/api/v1/platform/config/update"), None);
}
#[test]
fn scanner_includes_production_cfg_and_rejects_unresolved_forms() {
    let mut scanner = Scanner::default();
    scanner.visit_file(&syn::parse_file(r#"#[cfg(not(test))] fn production() { Router::new().route("/production", get(read)); } #[cfg(any(test, feature="optional"))] fn optional() { Router::new().route("/optional", post(write)); } #[cfg(all(test,feature="integration-tests"))] fn test_fixture() { Router::new().route("/excluded", get(read)); }"#).unwrap());
    assert_eq!(scanner.routes.len(), 2);
    for source in [
        r#"fn r() { Router::new().route(path, get(read)); }"#,
        r#"fn r() { Router::new().route("/a", custom_methods()); }"#,
        r#"fn r() { Router::new().nest("/api", named_router()); }"#,
        r#"fn r() { make!(Router::new().route("/hidden", get(read))); }"#,
    ] {
        assert!(
            std::panic::catch_unwind(
                || Scanner::default().visit_file(&syn::parse_file(source).unwrap())
            )
            .is_err()
        );
    }
}
#[test]
fn scanner_resolves_chained_methods_and_inline_nesting() {
    let file = syn::parse_file(r#"fn router() { Router::new().nest("/api", Router::new().route("/thing", get(read).post(write).delete(remove))); } #[cfg(test)] fn fake() { Router::new().route("/ignored", get(fake)); }"#).unwrap();
    let mut scanner = Scanner::default();
    scanner.visit_file(&file);
    assert_eq!(
        scanner.routes,
        ["GET", "POST", "DELETE"]
            .map(|m| (m.to_owned(), "/api/thing".to_owned()))
            .into_iter()
            .collect()
    );
}
