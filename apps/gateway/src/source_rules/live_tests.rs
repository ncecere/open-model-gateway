//! Inspect declarations even when the CI target or feature set disables them.
//! The shard inventory also checks macro-generated cases after expansion.
use proc_macro2::TokenTree;
use std::collections::BTreeSet;
use syn::ext::IdentExt;
use syn::{ItemFn, Meta, Token, parse::ParseStream, punctuated::Punctuated, visit::Visit};

#[derive(Default)]
struct Gates {
    tests: Vec<Vec<String>>,
    ignores: Vec<Vec<String>>,
}

impl Gates {
    fn inspect(&mut self, meta: &Meta, conditions: &mut Vec<String>, aliases: &AliasScope) {
        let path = meta.path();
        let single = path.get_ident().map(IdentExt::unraw);
        if single.as_ref().is_some_and(|name| name == "cfg_attr") {
            let (predicate, attrs) = meta
                .require_list()
                .unwrap()
                .parse_args_with(|input: ParseStream| {
                    // The predicate is one token tree (true/false), or a path
                    // with arguments or a value. Only the following attributes
                    // use the Meta grammar.
                    let mut predicate = proc_macro2::TokenStream::new();
                    while !input.peek(Token![,]) {
                        predicate.extend([input.parse::<TokenTree>()?]);
                    }
                    input.parse::<Token![,]>()?;
                    let attrs = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
                    Ok((predicate.to_string(), attrs))
                })
                .expect("valid cfg_attr metadata");
            let conditional = predicate != "true";
            if conditional {
                conditions.push(predicate);
            }
            for attr in &attrs {
                self.inspect(attr, conditions, aliases);
            }
            if conditional {
                conditions.pop();
            }
        } else if path
            .segments
            .last()
            .is_some_and(|s| s.ident.unraw() == "test")
            || aliases.matches(path)
        {
            self.tests.push(conditions.clone());
        } else if single.as_ref().is_some_and(|name| name == "ignore") {
            self.ignores.push(conditions.clone());
        }
    }

    fn has_ungated_test(&self, active: &[String]) -> bool {
        self.tests.iter().any(|test| {
            !self.ignores.iter().any(|ignore| {
                ignore
                    .iter()
                    .all(|condition| test.contains(condition) || active.contains(condition))
            })
        })
    }
}

#[derive(Default)]
struct Ungated {
    aliases: AliasScope,
    cases: Vec<String>,
    active: Vec<String>,
}

fn cfg_predicates(attrs: &[syn::Attribute]) -> impl Iterator<Item = String> {
    attrs.iter().filter_map(|attr| {
        let list = attr.meta.require_list().ok()?;
        list.path
            .get_ident()
            .is_some_and(|id| id.unraw() == "cfg")
            .then(|| list.tokens.to_string())
    })
}

type Names = BTreeSet<Vec<String>>;

fn segment(path: &mut Vec<String>, ident: &syn::Ident) {
    match ident.unraw().to_string().as_str() {
        "crate" => path.clear(),
        "super" => {
            path.pop();
        }
        "self" => {}
        name => path.push(name.to_owned()),
    }
}

// Flatten grouped imports to their source path and local binding. A glob
// imports the suffix below its module, with no local prefix.
fn imports(
    tree: &syn::UseTree,
    mut path: Vec<String>,
    bind: &mut impl FnMut(&[String], Option<String>),
) {
    match tree {
        syn::UseTree::Path(p) => {
            segment(&mut path, &p.ident);
            imports(&p.tree, path, bind);
        }
        syn::UseTree::Group(g) => {
            for item in &g.items {
                imports(item, path.clone(), bind);
            }
        }
        syn::UseTree::Name(n) => {
            segment(&mut path, &n.ident);
            bind(&path, path.last().cloned());
        }
        syn::UseTree::Rename(r) => {
            segment(&mut path, &r.ident);
            bind(&path, Some(r.rename.unraw().to_string()));
        }
        syn::UseTree::Glob(_) => bind(&path, None),
    }
}

#[derive(Default)]
struct AliasScope {
    module: Vec<String>,
    declared: Names,
    local: Names,
}

impl AliasScope {
    // A qualified attribute may reach an explicit re-export declared later.
    fn index(&mut self, items: &[syn::Item]) {
        for item in items {
            match item {
                syn::Item::Mod(m) => {
                    if let Some((_, nested)) = &m.content {
                        self.module.push(m.ident.unraw().to_string());
                        self.index(nested);
                        self.module.pop();
                    }
                }
                syn::Item::Use(u) => imports(&u.tree, self.module.clone(), &mut |path, binding| {
                    if path.last().is_some_and(|s| s == "test")
                        && let Some(binding) = binding
                    {
                        let mut name = self.module.clone();
                        name.push(binding);
                        self.declared.insert(name);
                    }
                }),
                _ => {}
            }
        }
    }

    fn import_item(&mut self, item: &syn::Item) {
        let syn::Item::Use(u) = item else { return };
        imports(&u.tree, self.module.clone(), &mut |path, binding| {
            let prefix: Vec<_> = binding.into_iter().collect();
            if path.last().is_some_and(|s| s == "test") && !prefix.is_empty() {
                self.local.insert(prefix.clone());
            }
            for name in &self.declared {
                if let Some(suffix) = name.strip_prefix(path) {
                    let mut local = prefix.clone();
                    local.extend_from_slice(suffix);
                    self.local.insert(local);
                }
            }
        });
    }

    fn matches(&self, path: &syn::Path) -> bool {
        let local: Vec<_> = path
            .segments
            .iter()
            .map(|s| s.ident.unraw().to_string())
            .collect();
        if self.local.contains(&local) {
            return true;
        }
        let mut qualified = self.module.clone();
        for s in &path.segments {
            segment(&mut qualified, &s.ident);
        }
        self.declared.contains(&qualified)
    }
}

impl<'ast> Visit<'ast> for Ungated {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let parent = std::mem::take(&mut self.aliases.local);
        self.aliases.module.push(item.ident.unraw().to_string());
        if let Some((_, items)) = &item.content {
            for item in items {
                self.aliases.import_item(item);
            }
        }
        let outer = self.active.len();
        self.active.extend(cfg_predicates(&item.attrs));
        syn::visit::visit_item_mod(self, item);
        self.aliases.module.pop();
        self.aliases.local = parent;
        self.active.truncate(outer);
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let outer = self.aliases.local.clone();
        for stmt in &block.stmts {
            if let syn::Stmt::Item(item) = stmt {
                self.aliases.import_item(item);
            }
        }
        syn::visit::visit_block(self, block);
        self.aliases.local = outer;
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        let mut gates = Gates::default();
        for attr in &item.attrs {
            gates.inspect(&attr.meta, &mut Vec::new(), &self.aliases);
        }
        let outer = self.active.len();
        self.active.extend(cfg_predicates(&item.attrs));
        if gates.has_ungated_test(&self.active) {
            self.cases.push(item.sig.ident.to_string());
        }
        syn::visit::visit_item_fn(self, item);
        self.active.truncate(outer);
    }
}

fn ungated(source: &str) -> Vec<String> {
    let mut cases = Ungated::default();
    let file = syn::parse_file(source).expect("valid live test source");
    cases.aliases.index(&file.items);
    for item in &file.items {
        cases.aliases.import_item(item);
    }
    cases.active.extend(cfg_predicates(&file.attrs));
    cases.visit_file(&file);
    cases.cases
}

#[test]
fn live_provider_tests_require_ignore() {
    let tests = super::tree::src_dir().with_file_name("tests");
    for entry in tests.read_dir().unwrap() {
        let path = entry.unwrap().path();
        if path.to_str().unwrap().ends_with("_live.rs") {
            let cases = ungated(&std::fs::read_to_string(&path).unwrap());
            assert!(
                cases.is_empty(),
                "{}: missing #[ignore]: {cases:?}",
                path.display()
            );
        }
    }
}

#[test]
fn ignore_applies_only_to_its_own_case() {
    let source = r#"
        #[test] #[ignore] fn opted_in() {}
        #[ignore = "requires operator opt-in"] #[tokio::test] async fn another() {}
        #[r#cfg_attr(unix, cfg_attr(feature = "paid", tokio::r#test))]
        async fn conditional_case() {}
        #[cfg_attr(unix, test, ignore)] fn conditional_ignored() {}
        #[cfg_attr(unix, test)] #[cfg_attr(unix, ignore)] fn matching_predicate() {}
        #[cfg_attr(unix, test)] #[cfg_attr(windows, ignore)] fn mismatched_predicate() {}
        #[cfg(unix)] #[test] #[cfg_attr(unix, ignore)] fn function_cfg() {}
        #[cfg_attr(true, test, ignore)] fn always_opted_in() {}
        #[cfg_attr(false, test, ignore)] fn never_built() {}
        #[cfg(unix)] mod platform {
            #[test] #[cfg_attr(unix, ignore)] fn module_cfg() {}
        }
        fn helper() {}
        mod nested {
            #[cfg(any())] #[tokio::test] async fn disabled_on_ci() {}
            /// Example: `#[ignore]`
            #[test] fn ordinary() {}
        }
    "#;
    assert_eq!(
        ungated(source),
        [
            "conditional_case",
            "mismatched_predicate",
            "disabled_on_ci",
            "ordinary"
        ]
    );
}

#[test]
fn test_attribute_aliases_follow_the_case_ignore_conditions() {
    let source = r#"
        #[r#probe] async fn missing() {}
        #[probe] #[ignore] async fn ignored() {}
        #[cfg_attr(unix, cfg_attr(feature = "paid", another, ignore))] async fn opted_in() {}
        #[cfg_attr(unix, another)] #[cfg_attr(windows, ignore)] async fn mismatch() {}
        use tokio::{test as probe, test::{self as another}};
        mod helpers { pub fn test() {} }
        use helpers::test as call_helper;
        fn ordinary() { call_helper(); }
        mod sibling {
            use super::helpers::test as ignore;
            fn calls_helper() { ignore(); }
        }
        mod other { use super::*; #[test] #[ignore] fn opted_in() {} }
        fn local() { use tokio::test as scoped; #[scoped] async fn block_case() {} }
        mod exported { pub use tokio::test as attribute; }
        use exported as renamed;
        #[renamed::attribute] async fn qualified() {}
        mod nested { use super::*; #[cfg_attr(unix, probe)] async fn inherited() {} }
    "#;
    assert_eq!(
        ungated(source),
        [
            "missing",
            "mismatch",
            "block_case",
            "qualified",
            "inherited"
        ]
    );
}
