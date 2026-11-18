//! Shared source inventory: filenames never confer test-only status.
//! File selection must stay inside `src`, and opaque constructs that could
//! select unscanned code fail the inventory before any consumer trusts it.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::{
    ext::IdentExt,
    spanned::Spanned,
    visit::{self, Visit},
};

pub(crate) fn test_only(attrs: &[syn::Attribute]) -> bool {
    fn requires_test(meta: &syn::Meta) -> bool {
        match meta {
            syn::Meta::Path(p) => p.is_ident("test"),
            syn::Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
                let items = list
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                    )
                    .unwrap();
                if list.path.is_ident("all") {
                    items.iter().any(requires_test)
                } else {
                    !items.is_empty() && items.iter().all(requires_test)
                }
            }
            _ => false,
        }
    }
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|meta| requires_test(&meta))
    })
}

pub(crate) struct Source {
    pub(crate) name: String,
    pub(crate) ast: syn::File,
}

#[derive(Default)]
struct Modules {
    root: PathBuf,
    production: BTreeSet<PathBuf>,
    tests: BTreeSet<PathBuf>,
    visited: BTreeSet<(PathBuf, bool, bool)>,
    includes: Vec<(PathBuf, PathBuf)>,
}
impl Modules {
    fn source_path(&self, path: &Path, kind: &str) -> PathBuf {
        let resolved = std::fs::canonicalize(path)
            .unwrap_or_else(|_| panic!("{kind}: cannot resolve {}", path.display()));
        assert!(
            resolved.starts_with(&self.root)
                && resolved.is_file()
                && path.extension().is_some_and(|e| e == "rs")
                && resolved.extension().is_some_and(|e| e == "rs"),
            "{kind}: {} must be an .rs file inside {}",
            path.display(),
            self.root.display()
        );
        resolved
    }

    fn load(&mut self, path: &Path, gated: bool, explicit: bool) {
        let path = self.source_path(path, "module path");
        // One file can be declared through both an ordinary and an explicit
        // path; those contexts can resolve its child modules differently.
        if !self.visited.insert((path.clone(), gated, explicit)) {
            return;
        }
        let seen = if gated {
            &mut self.tests
        } else {
            &mut self.production
        };
        seen.insert(path.clone());
        let ast = syn::parse_file(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let parent = path.parent().unwrap();
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let module_dir = if explicit || stem == "mod" {
            parent.to_owned()
        } else {
            parent.join(stem)
        };
        ModuleVisitor {
            modules: self,
            from: path.clone(),
            module_dir,
            path_dir: parent.to_owned(),
            gated,
        }
        .visit_file(&ast);
    }
}

/// Resolving module declarations also visits function bodies: Rust permits
/// file modules there, and their test gates must not hide a production file.
struct ModuleVisitor<'a> {
    modules: &'a mut Modules,
    from: PathBuf,
    module_dir: PathBuf,
    path_dir: PathBuf,
    gated: bool,
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::ExternCrate(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::ForeignMod(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::TraitAlias(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn expr_attrs(expr: &syn::Expr) -> &[syn::Attribute] {
    match expr {
        syn::Expr::Array(e) => &e.attrs,
        syn::Expr::Assign(e) => &e.attrs,
        syn::Expr::Async(e) => &e.attrs,
        syn::Expr::Await(e) => &e.attrs,
        syn::Expr::Binary(e) => &e.attrs,
        syn::Expr::Block(e) => &e.attrs,
        syn::Expr::Break(e) => &e.attrs,
        syn::Expr::Call(e) => &e.attrs,
        syn::Expr::Cast(e) => &e.attrs,
        syn::Expr::Closure(e) => &e.attrs,
        syn::Expr::Const(e) => &e.attrs,
        syn::Expr::Continue(e) => &e.attrs,
        syn::Expr::Field(e) => &e.attrs,
        syn::Expr::ForLoop(e) => &e.attrs,
        syn::Expr::Group(e) => &e.attrs,
        syn::Expr::If(e) => &e.attrs,
        syn::Expr::Index(e) => &e.attrs,
        syn::Expr::Infer(e) => &e.attrs,
        syn::Expr::Let(e) => &e.attrs,
        syn::Expr::Lit(e) => &e.attrs,
        syn::Expr::Loop(e) => &e.attrs,
        syn::Expr::Macro(e) => &e.attrs,
        syn::Expr::Match(e) => &e.attrs,
        syn::Expr::MethodCall(e) => &e.attrs,
        syn::Expr::Paren(e) => &e.attrs,
        syn::Expr::Path(e) => &e.attrs,
        syn::Expr::Range(e) => &e.attrs,
        syn::Expr::RawAddr(e) => &e.attrs,
        syn::Expr::Reference(e) => &e.attrs,
        syn::Expr::Repeat(e) => &e.attrs,
        syn::Expr::Return(e) => &e.attrs,
        syn::Expr::Struct(e) => &e.attrs,
        syn::Expr::Try(e) => &e.attrs,
        syn::Expr::TryBlock(e) => &e.attrs,
        syn::Expr::Tuple(e) => &e.attrs,
        syn::Expr::Unary(e) => &e.attrs,
        syn::Expr::Unsafe(e) => &e.attrs,
        syn::Expr::While(e) => &e.attrs,
        syn::Expr::Yield(e) => &e.attrs,
        _ => &[],
    }
}

fn named(path: &syn::Path, name: &str) -> bool {
    path.get_ident().is_some_and(|i| i.unraw() == name)
}

fn reject(from: &Path, span: Span, reason: &str) -> ! {
    let start = span.start();
    panic!(
        "{}:{}:{}: {reason}",
        from.display(),
        start.line,
        start.column + 1
    );
}

fn conditional_path(from: &Path, meta: &syn::Meta) -> bool {
    let syn::Meta::List(list) = meta else {
        return false;
    };
    if !named(&list.path, "cfg_attr") {
        return false;
    }
    let args = list
        .parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
        .unwrap_or_else(|e| reject(from, e.span(), "unreadable cfg_attr in source inventory"));
    args.iter()
        .skip(1)
        .any(|m| named(m.path(), "path") || conditional_path(from, m))
}

fn explicit_path(from: &Path, attrs: &[syn::Attribute]) -> Option<String> {
    let mut found = None;
    for attr in attrs {
        if conditional_path(from, &attr.meta) {
            reject(
                from,
                attr.span(),
                "cfg_attr module path is unsupported: use an unconditional #[path] or ordinary mod declaration",
            );
        }
        if !named(attr.path(), "path") {
            continue;
        }
        let syn::Meta::NameValue(value) = &attr.meta else {
            reject(from, attr.span(), "invalid module path");
        };
        let syn::Expr::Lit(value) = &value.value else {
            reject(from, attr.span(), "nonliteral module path");
        };
        let syn::Lit::Str(value) = &value.lit else {
            reject(from, attr.span(), "nonstring module path");
        };
        if found.replace(value.value()).is_some() {
            reject(from, attr.span(), "duplicate module path");
        }
    }
    found
}

/// Opaque macro tokens cannot select files. Inline item macros without file
/// selection tokens remain supported; strings and comments are data, not code.
fn check_macro_tokens(from: &Path, stream: TokenStream) {
    let tokens: Vec<_> = stream.into_iter().collect();
    for (at, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Ident(i) if i.unraw() == "include" => {
                reject(
                    from,
                    token.span(),
                    "include! inside a macro cannot be resolved by the source inventory",
                );
            }
            TokenTree::Ident(i) if i.unraw() == "mod" => {
                reject(
                    from,
                    token.span(),
                    "module tokens inside a macro are unsupported by the source inventory",
                );
            }
            TokenTree::Group(g) => check_macro_tokens(from, g.stream()),
            TokenTree::Punct(p)
                if p.as_char() == '$'
                    && matches!(tokens.get(at + 1), Some(TokenTree::Ident(_)))
                    && matches!(tokens.get(at + 2), Some(TokenTree::Punct(p)) if p.as_char() == '!') =>
            {
                reject(
                    from,
                    token.span(),
                    "computed macro call is unsupported by the source inventory",
                );
            }
            _ => {}
        }
    }
}

impl ModuleVisitor<'_> {
    fn under_attrs(&mut self, attrs: &[syn::Attribute], visit: impl FnOnce(&mut Self)) {
        let outer = self.gated;
        self.gated |= test_only(attrs);
        visit(self);
        self.gated = outer;
    }
}

impl<'ast> Visit<'ast> for ModuleVisitor<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        self.under_attrs(item_attrs(item), |v| visit::visit_item(v, item));
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.under_attrs(&item.attrs, |v| visit::visit_impl_item_fn(v, item));
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.under_attrs(&item.attrs, |v| visit::visit_trait_item_fn(v, item));
    }

    fn visit_impl_item_const(&mut self, item: &'ast syn::ImplItemConst) {
        self.under_attrs(&item.attrs, |v| visit::visit_impl_item_const(v, item));
    }

    fn visit_trait_item_const(&mut self, item: &'ast syn::TraitItemConst) {
        self.under_attrs(&item.attrs, |v| visit::visit_trait_item_const(v, item));
    }

    fn visit_expr(&mut self, expr: &'ast syn::Expr) {
        self.under_attrs(expr_attrs(expr), |v| visit::visit_expr(v, expr));
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        self.under_attrs(&local.attrs, |v| visit::visit_local(v, local));
    }

    fn visit_stmt_macro(&mut self, stmt: &'ast syn::StmtMacro) {
        self.under_attrs(&stmt.attrs, |v| visit::visit_stmt_macro(v, stmt));
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        self.under_attrs(&arm.attrs, |v| visit::visit_arm(v, arm));
    }

    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        self.under_attrs(&field.attrs, |v| visit::visit_field_value(v, field));
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let name = module.ident.unraw().to_string();
        let explicit = explicit_path(&self.from, &module.attrs);
        if let Some((_, items)) = &module.content {
            let dir = explicit.map_or_else(
                || self.module_dir.join(&name),
                |path| self.path_dir.join(path),
            );
            let mut nested = ModuleVisitor {
                modules: self.modules,
                from: self.from.clone(),
                module_dir: dir.clone(),
                path_dir: dir,
                gated: self.gated,
            };
            for item in items {
                nested.visit_item(item);
            }
            return;
        }
        let has_path = explicit.is_some();
        let path = explicit.map_or_else(
            || {
                let flat = self.module_dir.join(format!("{name}.rs"));
                let nested = self.module_dir.join(&name).join("mod.rs");
                assert_ne!(
                    flat.exists(),
                    nested.exists(),
                    "missing or ambiguous module {name} in {}",
                    self.module_dir.display()
                );
                if flat.exists() { flat } else { nested }
            },
            |path| self.path_dir.join(path),
        );
        self.modules.load(&path, self.gated, has_path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if self.gated {
            return;
        }
        if mac
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident.unraw() == "include")
        {
            let target = syn::parse2::<syn::LitStr>(mac.tokens.clone()).unwrap_or_else(|_| {
                reject(
                    &self.from,
                    mac.span(),
                    "include! requires a literal scanned .rs path",
                )
            });
            let target = self.modules.source_path(
                &self.from.parent().unwrap().join(target.value()),
                "include!",
            );
            self.modules.includes.push((self.from.clone(), target));
        } else {
            check_macro_tokens(&self.from, mac.tokens.clone());
        }
    }

    fn visit_use_tree(&mut self, tree: &'ast syn::UseTree) {
        if self.gated {
            return;
        }
        let ident = match tree {
            syn::UseTree::Name(n) => Some(&n.ident),
            syn::UseTree::Rename(n) => Some(&n.ident),
            _ => None,
        };
        if let Some(name) = ident.filter(|i| i.unraw() == "include") {
            reject(
                &self.from,
                name.span(),
                "include! import/alias is unsupported by the source inventory",
            );
        }
        visit::visit_use_tree(self, tree);
    }
}

pub(crate) fn sources_at(root: &Path) -> Vec<Source> {
    fn files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            // Aliases and directory cycles make canonical membership ambiguous.
            assert!(
                !entry.file_type().unwrap().is_symlink(),
                "source inventory does not support symlinks: {}",
                entry.path().display()
            );
            let path = entry.path();
            if path.is_dir() {
                files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = std::fs::canonicalize(root).unwrap();
    let mut modules = Modules {
        root: root.clone(),
        ..Modules::default()
    };
    for name in ["lib.rs", "main.rs"] {
        let path = root.join(name);
        if path.exists() {
            modules.load(&path, false, true);
        }
    }
    let mut all = Vec::new();
    files(&root, &mut all);
    all.sort();
    // Cargo treats bin/name.rs and bin/name/main.rs as crate roots. Resolve
    // their children in that context even if a test also references the file.
    let bin = root.join("bin");
    for path in &all {
        let parent = path.parent();
        if parent == Some(bin.as_path())
            || (path.file_name().is_some_and(|name| name == "main.rs")
                && parent.and_then(Path::parent) == Some(bin.as_path()))
        {
            modules.load(path, false, true);
        }
    }
    // Unreferenced .rs files are conservatively production too. Inspect their
    // declarations and macros before trusting the same coverage on them.
    for path in &all {
        if !modules.tests.contains(path) && !modules.production.contains(path) {
            modules.load(path, false, false);
        }
    }
    let scanned: BTreeSet<_> = all
        .into_iter()
        .filter(|path| !modules.tests.contains(path) || modules.production.contains(path))
        .collect();
    for (from, target) in &modules.includes {
        assert!(
            scanned.contains(target),
            "{}: include! target {} is not a scanned production file",
            from.display(),
            target.display()
        );
        // A file module's default directory depends on its inclusion context.
        // Ordinary included items are already scanned; these need a resolver
        // that follows the compiler's context before they can be supported.
        struct IncludedModules;
        impl<'ast> Visit<'ast> for IncludedModules {
            fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
                assert!(
                    module.content.is_some(),
                    "include! target with file modules requires contextual inventory support"
                );
                visit::visit_item_mod(self, module);
            }
        }
        IncludedModules
            .visit_file(&syn::parse_file(&std::fs::read_to_string(target).unwrap()).unwrap());
    }
    scanned
        .into_iter()
        .map(|path| Source {
            name: path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
            ast: syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap(),
        })
        .collect()
}

pub(crate) fn production_sources() -> Vec<Source> {
    sources_at(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"))
}

#[test]
fn exclusions_follow_module_gates_not_filenames_or_directory_names() {
    let dir = tempfile::tempdir().unwrap();
    for (name, source) in [
        (
            "lib.rs",
            "mod harmless_tests; mod shared; #[cfg(test)] #[path=\"shared.rs\"] mod also_test; #[cfg(all(test, feature=\"integration-tests\"))] #[path=\"odd.rs\"] mod gated;",
        ),
        ("harmless_tests.rs", "fn production() {}"),
        ("shared.rs", "fn production() {}"),
        ("odd.rs", "#[path=\"odd/child.rs\"] mod child;"),
        ("odd/child.rs", "fn test_helper() {}"),
        ("tests.rs", "fn orphan() {}"),
        ("tests/payload.rs", "fn orphan() {}"),
    ] {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let names: BTreeSet<_> = sources_at(dir.path()).into_iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        [
            "lib.rs",
            "harmless_tests.rs",
            "shared.rs",
            "tests.rs",
            "tests/payload.rs"
        ]
        .map(str::to_owned)
        .into_iter()
        .collect()
    );
    let module: syn::ItemMod = syn::parse_str(
        "#[cfg(any(test, feature=\"integration-tests\"))] mod usable_in_production;",
    )
    .unwrap();
    assert!(!test_only(&module.attrs));
}

#[cfg(test)]
mod coverage {
    use super::*;

    fn fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn refused(root: &Path, reason: &str) {
        let failure = std::panic::catch_unwind(|| sources_at(root))
            .err()
            .expect("unsupported source layout must fail closed");
        let message = failure
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| failure.downcast_ref::<&str>().copied())
            .unwrap();
        assert!(message.contains(reason), "{message:?} must name {reason:?}");
    }

    #[test]
    fn conditional_module_paths_fail_closed() {
        for declaration in [
            "#[cfg_attr(all(), path = \"alternate.rs\")] mod ordinary;",
            "#[r#cfg_attr(all(), r#path = \"alternate.rs\")] mod ordinary;",
            "#[cfg_attr(feature = \"optional\", cfg_attr(unix, path = \"alternate.rs\"))] mod ordinary;",
            "mod inline { #[cfg_attr(unix, path = \"alternate.rs\")] mod ordinary; }",
        ] {
            let dir = fixture(&[
                ("lib.rs", declaration),
                ("ordinary.rs", "fn ordinary() {}"),
                ("alternate.rs", "fn alternate() {}"),
                ("inline/ordinary.rs", "fn ordinary() {}"),
            ]);
            refused(dir.path(), "cfg_attr");
        }
    }

    #[test]
    fn test_only_body_gates_apply_only_to_their_own_scope() {
        for layout in [
            r#"GATE { include!("helper.inc"); }"#,
            r#"GATE let () = include!("helper.inc");"#,
            r#"GATE include!("helper.inc");"#,
            r#"GATE if true { include!("helper.inc"); }"#,
            r#"match true { GATE true => include!("helper.inc"), _ => () }"#,
            r#"let _ = Value { GATE field: include!("helper.inc"), ..fallback };"#,
        ] {
            let dir = fixture(&[("helper.inc", "()")]);
            let entry = dir.path().join("lib.rs");
            let gated = layout.replace("GATE", "#[cfg(test)]");
            let live = layout.replace("GATE", "");
            let definition = "struct Value { field: () } fn live(fallback: Value)";
            std::fs::write(&entry, format!("{definition} {{ {gated} }}")).unwrap();
            assert_eq!(sources_at(dir.path()).len(), 1);

            // The identical layout following its gated sibling is production.
            std::fs::write(&entry, format!("{definition} {{ {gated} {live} }}")).unwrap();
            refused(dir.path(), "include!");
        }
    }

    #[test]
    fn test_only_associated_constants_keep_their_gate() {
        for declaration in [
            r#"struct Owner; impl Owner { GATE const VALUE: () = include!("helper.inc"); }"#,
            r#"trait Owner { GATE const VALUE: () = include!("helper.inc"); }"#,
        ] {
            let dir = fixture(&[("helper.inc", "()")]);
            let entry = dir.path().join("lib.rs");
            std::fs::write(&entry, declaration.replace("GATE", "#[cfg(test)]")).unwrap();
            assert_eq!(sources_at(dir.path()).len(), 1);
            std::fs::write(&entry, declaration.replace("GATE", "")).unwrap();
            refused(dir.path(), "include!");
        }
    }

    #[test]
    fn conventional_cargo_binaries_resolve_children_from_the_crate_root() {
        let dir = fixture(&[
            (
                "lib.rs",
                r#"#[cfg(test)] #[path = "bin/tool/helper.rs"] mod test_copy;"#,
            ),
            ("bin/tool/main.rs", "mod helper; fn main() {}"),
            ("bin/tool/helper.rs", "fn marker() {}"),
            ("bin/flat.rs", "mod shared; fn main() {}"),
            ("bin/shared/mod.rs", "fn marker() {}"),
        ]);
        let names: BTreeSet<_> = sources_at(dir.path()).into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            BTreeSet::from([
                "lib.rs".into(),
                "bin/tool/main.rs".into(),
                "bin/tool/helper.rs".into(),
                "bin/flat.rs".into(),
                "bin/shared/mod.rs".into(),
            ])
        );
    }

    #[test]
    fn unsupported_constructs_report_the_source_line_and_column() {
        for (declaration, diagnostic) in [
            (
                "    #[cfg_attr(unix, path = \"other.rs\")] mod ordinary;",
                "helper.rs:3:5: cfg_attr",
            ),
            (
                "    macro_rules! modules { () => { mod helper; } }",
                "helper.rs:3:36: module tokens",
            ),
        ] {
            let dir = fixture(&[
                ("lib.rs", "mod helper;"),
                ("helper.rs", &format!("// header\n\n{declaration}")),
            ]);
            refused(dir.path(), diagnostic);
        }
    }

    #[test]
    fn includes_must_name_scanned_production_rust_files() {
        for source in [
            "include!(\"helper.inc\");",
            "fn value() { std::include!(\"helper.inc\"); }",
            "include!(\"tests.rs\"); #[cfg(test)] mod tests;",
            "include!(concat!(\"helper\", \".rs\"));",
            "include!(\"../outside.rs\");",
            "use std::{include as load}; load!(\"helper.rs\");",
            "include!(\"missing.rs\");",
        ] {
            let dir = fixture(&[
                ("src/lib.rs", source),
                ("src/helper.inc", "fn helper() {}"),
                ("src/helper.rs", "fn helper() {}"),
                ("src/tests.rs", "fn helper() {}"),
                ("outside.rs", "fn helper() {}"),
            ]);
            refused(&dir.path().join("src"), "include!");
        }
    }

    #[test]
    fn included_module_declarations_require_contextual_inventory_support() {
        let dir = fixture(&[
            ("lib.rs", "include!(\"helper.rs\");"),
            ("helper.rs", "mod child;"),
            ("helper/child.rs", "fn marker() {}"),
            ("child.rs", "fn marker() {}"),
        ]);
        refused(dir.path(), "include!");
    }

    #[test]
    fn module_paths_must_stay_in_rust_source_tree() {
        for target in ["../outside.rs", "helper.inc", "helper.RS"] {
            let dir = fixture(&[
                ("src/lib.rs", &format!("#[path = {target:?}] mod helper;")),
                ("outside.rs", "fn helper() {}"),
                ("src/helper.inc", "fn helper() {}"),
                ("src/helper.RS", "fn helper() {}"),
            ]);
            refused(&dir.path().join("src"), "module path");
        }
    }

    #[test]
    fn opaque_macros_cannot_add_uninventoried_files() {
        for source in [
            "macro_rules! declare { () => { mod helper; } }",
            "macro_rules! declare { ($name:ident) => { pub(crate) mod $name; } }",
            "macro_rules! declare { () => { mod inline { mod helper; } } }",
            "macro_rules! declare { () => { include!(\"helper.rs\"); } }",
            "forward! { include!(\"helper.rs\"); }",
            "declare!(mod);",
            "macro_rules! declare { ($call:ident) => { $call!(\"helper.rs\"); } }",
        ] {
            let dir = fixture(&[("lib.rs", source), ("helper.rs", "fn helper() {}")]);
            refused(dir.path(), "macro");
        }
    }

    #[test]
    fn supported_layouts_keep_complete_production_coverage() {
        let dir = fixture(&[
            (
                "lib.rs",
                r#"
                mod flat;
                mod nested;
                mod inline { #[path = "renamed.rs"] mod child; }
                #[path = "custom/location.rs"] mod renamed;
                #[path = "custom"] mod redirected { mod child; }
                mod r#type;
                #[cfg(test)] mod tests;
                #[cfg(test)] #[path = "shared.rs"] mod test_shared;
                include!("shared.rs");
                include!("orphan.rs");
                #[cfg_attr(unix, allow(dead_code))] fn ordinary() {}
                macro_rules! items { () => { fn ordinary() {} } }
                const TEXT: &str = "include!(\"data\"); mod data;";
                // include!("data"); mod data;
            "#,
            ),
            ("main.rs", "#[path = \"shared.rs\"] mod binary_shared;"),
            ("flat.rs", "mod child;"),
            ("flat/child.rs", "fn marker() {}"),
            ("nested/mod.rs", "mod child;"),
            ("nested/child.rs", "fn marker() {}"),
            ("inline/renamed.rs", "fn marker() {}"),
            ("custom/location.rs", "fn marker() {}"),
            ("custom/child.rs", "fn marker() {}"),
            ("type.rs", "fn marker() {}"),
            ("shared.rs", "fn marker() {}"),
            ("orphan.rs", "fn marker() {}"),
            ("tests.rs", "include!(\"test-data.inc\");"),
            ("test-data.inc", "fn helper() {}"),
        ]);
        let sources = sources_at(dir.path());
        let names: BTreeSet<_> = sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            BTreeSet::from([
                "lib.rs",
                "main.rs",
                "flat.rs",
                "flat/child.rs",
                "nested/mod.rs",
                "nested/child.rs",
                "inline/renamed.rs",
                "custom/location.rs",
                "custom/child.rs",
                "type.rs",
                "shared.rs",
                "orphan.rs",
            ])
        );
        // Consumers receive parsed bodies, not just a list of matching names.
        for source in sources.iter().filter(|s| s.name.ends_with("child.rs")) {
            assert!(source.ast.items.iter().any(|item| matches!(item,
                syn::Item::Fn(f) if f.sig.ident == "marker")));
        }
    }

    #[test]
    fn production_declarations_in_bodies_and_orphans_are_inspected() {
        for source in [
            "fn ordinary() { #[path = \"../outside.rs\"] mod helper; }",
            "#[cfg(test)] fn helper() { #[path = \"shared.rs\"] mod test_copy; } #[path = \"shared.rs\"] mod live;",
        ] {
            let dir = fixture(&[
                ("src/lib.rs", source),
                ("src/shared.rs", "fn marker() {}"),
                ("outside.rs", "fn marker() {}"),
            ]);
            if source.starts_with("fn ordinary") {
                refused(&dir.path().join("src"), "module path");
            } else {
                let names: Vec<_> = sources_at(&dir.path().join("src"))
                    .into_iter()
                    .map(|s| s.name)
                    .collect();
                assert!(names.contains(&"shared.rs".to_owned()));
            }
        }
        let dir = fixture(&[
            ("lib.rs", "fn marker() {}"),
            ("orphan.rs", "include!(\"helper.inc\");"),
            ("helper.inc", "fn helper() {}"),
        ]);
        refused(dir.path(), "include!");
    }

    #[test]
    fn absolute_module_paths_obey_the_same_boundary() {
        let dir = fixture(&[
            ("src/lib.rs", ""),
            ("src/helper.rs", "fn marker() {}"),
            ("outside.rs", "fn marker() {}"),
        ]);
        for (target, allowed) in [("src/helper.rs", true), ("outside.rs", false)] {
            std::fs::write(
                dir.path().join("src/lib.rs"),
                format!(
                    "#[path = {:?}] mod helper;",
                    dir.path().join(target).to_str().unwrap()
                ),
            )
            .unwrap();
            if allowed {
                assert_eq!(sources_at(&dir.path().join("src")).len(), 2);
            } else {
                refused(&dir.path().join("src"), "module path");
            }
        }
    }

    #[test]
    fn explicit_file_paths_resolve_children_from_the_parent_directory() {
        let dir = fixture(&[
            ("lib.rs", "#[path = \"custom/location.rs\"] mod renamed;"),
            ("custom/location.rs", "mod child;"),
            ("custom/child.rs", "fn marker() {}"),
        ]);
        let names: BTreeSet<_> = sources_at(dir.path()).into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            BTreeSet::from([
                "lib.rs".to_owned(),
                "custom/location.rs".to_owned(),
                "custom/child.rs".to_owned(),
            ])
        );
    }

    #[test]
    fn repeated_files_keep_each_module_resolution_context() {
        let dir = fixture(&[
            (
                "lib.rs",
                r#"mod shared; #[path = "shared.rs"] mod explicit;
                #[cfg(test)] #[path = "child.rs"] mod test_copy;"#,
            ),
            ("shared.rs", "mod child;"),
            ("shared/child.rs", "fn marker() {}"),
            ("child.rs", "fn marker() {}"),
        ]);
        let names: BTreeSet<_> = sources_at(dir.path()).into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            BTreeSet::from([
                "lib.rs".to_owned(),
                "shared.rs".to_owned(),
                "shared/child.rs".to_owned(),
                "child.rs".to_owned(),
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_or_cycle_the_source_walk() {
        let dir = fixture(&[("src/lib.rs", ""), ("outside.rs", "fn marker() {}")]);
        std::os::unix::fs::symlink(
            dir.path().join("outside.rs"),
            dir.path().join("src/alias.rs"),
        )
        .unwrap();
        refused(&dir.path().join("src"), "symlinks");
        std::fs::remove_file(dir.path().join("src/alias.rs")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("src"), dir.path().join("src/cycle")).unwrap();
        refused(&dir.path().join("src"), "symlinks");
    }
}
