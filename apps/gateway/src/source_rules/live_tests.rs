//! Inspect declarations even when the CI target or feature set disables them.
//! The shard inventory also checks macro-generated cases after expansion.
use quote::ToTokens;
use syn::ext::IdentExt;
use syn::{ItemFn, Meta, Token, punctuated::Punctuated, visit::Visit};

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
            let args = meta
                .require_list()
                .unwrap()
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .expect("valid cfg_attr metadata");
            let mut args = args.iter();
            let condition = args.next().expect("cfg_attr predicate");
            conditions.push(condition.to_token_stream().to_string());
            for attr in args {
                self.inspect(attr, conditions);
            }
            conditions.pop();
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

    fn has_ungated_test(&self) -> bool {
        self.tests.iter().any(|test| {
            !self
                .ignores
                .iter()
                .any(|ignore| ignore.iter().all(|condition| test.contains(condition)))
        })
    }
}

#[derive(Default)]
struct Ungated(Vec<String>);

impl<'ast> Visit<'ast> for Ungated {
    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        let mut gates = Gates::default();
        for attr in &item.attrs {
            gates.inspect(&attr.meta, &mut Vec::new());
        }
        if gates.has_ungated_test() {
            self.0.push(item.sig.ident.to_string());
        }
        syn::visit::visit_item_fn(self, item);
    }
}

fn ungated(source: &str) -> Vec<String> {
    let mut cases = Ungated::default();
    cases.visit_file(&syn::parse_file(source).expect("valid live test source"));
    cases.0
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
