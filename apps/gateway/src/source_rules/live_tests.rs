//! Inspect declarations even when the CI target or feature set disables them.
//! The shard inventory also checks macro-generated cases after expansion.
use proc_macro2::TokenTree;
use syn::ext::IdentExt;
use syn::{ItemFn, Meta, Token, parse::ParseStream, punctuated::Punctuated, visit::Visit};

#[derive(Default)]
struct Gates {
    tests: Vec<Vec<String>>,
    ignores: Vec<Vec<String>>,
}

impl Gates {
    fn inspect(&mut self, meta: &Meta, conditions: &mut Vec<String>) {
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
                self.inspect(attr, conditions);
            }
            if conditional {
                conditions.pop();
            }
        } else if single.as_ref().is_some_and(|name| name == "ignore") {
            self.ignores.push(conditions.clone());
        } else if path
            .segments
            .last()
            .is_some_and(|s| s.ident.unraw() == "test")
        {
            self.tests.push(conditions.clone());
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

impl<'ast> Visit<'ast> for Ungated {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let outer = self.active.len();
        self.active.extend(cfg_predicates(&item.attrs));
        syn::visit::visit_item_mod(self, item);
        self.active.truncate(outer);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        let mut gates = Gates::default();
        for attr in &item.attrs {
            gates.inspect(&attr.meta, &mut Vec::new());
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
