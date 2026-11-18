//! Inspect declarations even when the CI target or feature set disables them.
//! The shard inventory also checks macro-generated cases after expansion.
use syn::{ItemFn, visit::Visit};

#[derive(Default)]
struct Ungated(Vec<String>);

impl<'ast> Visit<'ast> for Ungated {
    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        let is_test = item.attrs.iter().any(|attr| {
            attr.path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "test")
        });
        if is_test && !item.attrs.iter().any(|attr| attr.path().is_ident("ignore")) {
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
        fn helper() {}
        mod nested {
            #[cfg(any())] #[tokio::test] async fn disabled_on_ci() {}
            /// Example: `#[ignore]`
            #[test] fn ordinary() {}
        }
    "#;
    assert_eq!(ungated(source), ["disabled_on_ci", "ordinary"]);
}
