//! Pin every unsafe keyword/allowance in production, not just the default lint.
use crate::configuration_source_tests::{production_sources, test_only};
use std::collections::BTreeMap;
use syn::visit::{self, Visit};

type Sites = BTreeMap<(String, String, &'static str), usize>;
struct Inventory {
    file: String,
    function: String,
    sites: Sites,
}
impl Inventory {
    fn record(&mut self, kind: &'static str) {
        *self
            .sites
            .entry((self.file.clone(), self.function.clone(), kind))
            .or_default() += 1;
    }
    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        let mut attribute = false;
        for token in tokens {
            match token {
                proc_macro2::TokenTree::Ident(i) if i == "unsafe" => {
                    self.record("unsafe_macro");
                    attribute = false;
                }
                proc_macro2::TokenTree::Punct(p) if p.as_char() == '#' => attribute = true,
                proc_macro2::TokenTree::Punct(p) if p.as_char() == '!' && attribute => {}
                proc_macro2::TokenTree::Group(g) => {
                    if attribute
                        && g.delimiter() == proc_macro2::Delimiter::Bracket
                        && let Ok(meta) = syn::parse2::<syn::Meta>(g.stream())
                    {
                        self.attribute(&meta);
                    }
                    self.tokens(g.stream());
                    attribute = false;
                }
                _ => attribute = false,
            }
        }
    }
    fn attribute(&mut self, meta: &syn::Meta) {
        if let syn::Meta::List(list) = meta {
            let words: Vec<String> = list
                .tokens
                .to_string()
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .map(str::to_owned)
                .collect();
            if words.iter().any(|w| w == "unsafe_code")
                && (meta.path().is_ident("allow") || words.iter().any(|w| w == "allow"))
            {
                self.record("allow_unsafe");
            }
        }
        if meta.path().is_ident("unsafe") {
            self.record("unsafe_attribute");
        }
    }
}
impl<'ast> Visit<'ast> for Inventory {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !test_only(&node.attrs) {
            visit::visit_item_mod(self, node);
        }
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if test_only(&node.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, node.sig.ident.to_string());
        visit::visit_item_fn(self, node);
        self.function = previous;
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if test_only(&node.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, node.sig.ident.to_string());
        visit::visit_impl_item_fn(self, node);
        self.function = previous;
    }
    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if test_only(&node.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, node.sig.ident.to_string());
        visit::visit_trait_item_fn(self, node);
        self.function = previous;
    }
    fn visit_foreign_item_fn(&mut self, node: &'ast syn::ForeignItemFn) {
        if test_only(&node.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, node.sig.ident.to_string());
        visit::visit_foreign_item_fn(self, node);
        self.function = previous;
    }
    fn visit_signature(&mut self, node: &'ast syn::Signature) {
        if matches!(node.safety, syn::Safety::Unsafe(_)) {
            self.record("unsafe_fn");
        }
        visit::visit_signature(self, node);
    }
    fn visit_expr_unsafe(&mut self, node: &'ast syn::ExprUnsafe) {
        self.record("unsafe_block");
        visit::visit_expr_unsafe(self, node);
    }
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if test_only(&node.attrs) {
            return;
        }
        if node.unsafety.is_some() {
            self.record("unsafe_impl");
        }
        visit::visit_item_impl(self, node);
    }
    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if test_only(&node.attrs) {
            return;
        }
        if node.unsafety.is_some() {
            self.record("unsafe_trait");
        }
        visit::visit_item_trait(self, node);
    }
    fn visit_item_foreign_mod(&mut self, node: &'ast syn::ItemForeignMod) {
        if test_only(&node.attrs) {
            return;
        }
        if node.unsafety.is_some() {
            self.record("unsafe_extern");
        }
        visit::visit_item_foreign_mod(self, node);
    }
    fn visit_type_fn_ptr(&mut self, node: &'ast syn::TypeFnPtr) {
        if node.unsafety.is_some() {
            self.record("unsafe_fn_type");
        }
        visit::visit_type_fn_ptr(self, node);
    }
    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        self.attribute(&node.meta);
        visit::visit_attribute(self, node);
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.tokens(node.tokens.clone());
        visit::visit_macro(self, node);
    }
}

#[test]
fn unsafe_sites_are_exactly_the_reviewed_set() {
    let mut actual = Sites::new();
    for source in production_sources() {
        let mut inventory = Inventory {
            file: source.name,
            function: "<module>".into(),
            sites: Sites::new(),
        };
        inventory.visit_file(&source.ast);
        actual.extend(inventory.sites);
    }
    let expected = [
        ("startup.rs", "restrict_umask", "allow_unsafe", 1),
        ("startup.rs", "restrict_umask", "unsafe_block", 1),
        ("startup.rs", "import_secret_files", "allow_unsafe", 1),
        ("startup.rs", "import_secret_files", "unsafe_fn", 1),
        ("startup.rs", "import_secret_files", "unsafe_block", 1),
        ("filestore/local.rs", "open", "allow_unsafe", 1),
        ("filestore/local.rs", "open", "unsafe_block", 1),
        ("main.rs", "main", "allow_unsafe", 1),
        ("main.rs", "main", "unsafe_block", 1),
    ]
    .into_iter()
    .map(|(file, function, kind, n)| ((file.to_owned(), function.to_owned(), kind), n))
    .collect();
    assert_eq!(
        actual, expected,
        "new unsafe site or allowance requires explicit review"
    );
}

#[test]
fn inventory_counts_new_methods_signatures_attributes_and_macro_unsafe() {
    let source = r#"
        struct Example;
        impl Example { #[allow(unsafe_code)] unsafe fn extra() { unsafe { work(); } emit!(#[allow(unsafe_code)] unsafe { work(); }); } }
        #[cfg(test)] mod tests { unsafe fn ignored() {} }
    "#;
    let mut inventory = Inventory {
        file: "new.rs".into(),
        function: "<module>".into(),
        sites: Sites::new(),
    };
    inventory.visit_file(&syn::parse_file(source).unwrap());
    assert_eq!(inventory.sites.len(), 4);
    for kind in ["allow_unsafe", "unsafe_fn", "unsafe_block", "unsafe_macro"] {
        assert_eq!(
            inventory
                .sites
                .get(&("new.rs".into(), "extra".into(), kind)),
            Some(if kind == "allow_unsafe" { &2 } else { &1 })
        );
    }
}

#[test]
fn inventory_distinguishes_unsafe_safe_and_default_signatures() {
    let source = r#"
        unsafe fn free() {}
        fn safe_free() {}
        struct Example;
        impl Example {
            unsafe fn method() {}
            fn safe_method() {}
        }
        trait ExampleTrait {
            unsafe fn trait_method();
            fn safe_trait_method();
        }
        unsafe extern "C" {
            unsafe fn foreign();
            safe fn safe_foreign();
            fn default_foreign();
        }
    "#;
    let mut inventory = Inventory {
        file: "signatures.rs".into(),
        function: "<module>".into(),
        sites: Sites::new(),
    };
    inventory.visit_file(&syn::parse_file(source).unwrap());
    let expected = [
        ("free", "unsafe_fn"),
        ("method", "unsafe_fn"),
        ("trait_method", "unsafe_fn"),
        ("foreign", "unsafe_fn"),
        ("<module>", "unsafe_extern"),
    ]
    .into_iter()
    .map(|(function, kind)| (("signatures.rs".into(), function.into(), kind), 1))
    .collect();
    assert_eq!(inventory.sites, expected);
}

#[test]
fn inventory_counts_unsafe_function_pointer_aliases_and_nested_pointers() {
    for (source, count) in [
        ("type Callback = unsafe fn();", 1),
        (
            "type Callback = fn(unsafe fn()) -> unsafe extern \"C\" fn();",
            2,
        ),
        ("type Callback = fn(extern \"C\" fn()) -> fn();", 0),
    ] {
        let mut inventory = Inventory {
            file: "pointers.rs".into(),
            function: "<module>".into(),
            sites: Sites::new(),
        };
        inventory.visit_file(&syn::parse_file(source).unwrap());
        let mut expected = Sites::new();
        if count > 0 {
            expected.insert(
                ("pointers.rs".into(), "<module>".into(), "unsafe_fn_type"),
                count,
            );
        }
        assert_eq!(inventory.sites, expected, "{source}");
    }
}

#[test]
fn inventory_counts_unsafe_traits_impls_and_extern_blocks() {
    let source = r#"
        struct Example;
        unsafe trait Unchecked {}
        unsafe impl Unchecked for Example {}
        unsafe extern "C" {}
        trait Checked {}
        impl Checked for Example {}
        extern "C" {}
    "#;
    let mut inventory = Inventory {
        file: "items.rs".into(),
        function: "<module>".into(),
        sites: Sites::new(),
    };
    inventory.visit_file(&syn::parse_file(source).unwrap());
    let expected = ["unsafe_trait", "unsafe_impl", "unsafe_extern"]
        .into_iter()
        .map(|kind| (("items.rs".into(), "<module>".into(), kind), 1))
        .collect();
    assert_eq!(inventory.sites, expected);
}

#[test]
fn inventory_preserves_macro_traversal_and_nested_test_skipping() {
    let source = r#"
        fn outer() {
            unsafe fn nested() {}
            emit!({ #[allow(unsafe_code)] unsafe {} });
            #[cfg(test)] unsafe fn ignored() { emit!(unsafe {}); }
        }
        mod production {
            #[cfg(test)] mod tests {
                mod nested {
                    unsafe fn ignored() {}
                    type Callback = unsafe fn();
                    emit!(#[allow(unsafe_code)] unsafe {});
                }
            }
        }
        struct Example;
        impl Example { #[cfg(test)] unsafe fn ignored_method() {} }
        trait ExampleTrait { #[cfg(test)] unsafe fn ignored_trait_method(); }
        extern "C" { #[cfg(test)] unsafe fn ignored_foreign(); }
        #[cfg(test)] unsafe trait IgnoredTrait {}
        #[cfg(test)] unsafe impl IgnoredTrait for Example {}
        #[cfg(test)] unsafe extern "C" { unsafe fn ignored_block(); }
    "#;
    let mut inventory = Inventory {
        file: "nested.rs".into(),
        function: "<module>".into(),
        sites: Sites::new(),
    };
    inventory.visit_file(&syn::parse_file(source).unwrap());
    let expected = [
        ("nested", "unsafe_fn"),
        ("outer", "allow_unsafe"),
        ("outer", "unsafe_macro"),
    ]
    .into_iter()
    .map(|(function, kind)| (("nested.rs".into(), function.into(), kind), 1))
    .collect();
    assert_eq!(inventory.sites, expected);
}
