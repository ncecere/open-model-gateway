//! Shared source inventory: filenames never confer test-only status.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
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
    production: BTreeSet<PathBuf>,
    tests: BTreeSet<PathBuf>,
}
impl Modules {
    fn load(&mut self, path: &Path, gated: bool) {
        let path = std::fs::canonicalize(path)
            .unwrap_or_else(|_| panic!("unresolved module {}", path.display()));
        let seen = if gated {
            &mut self.tests
        } else {
            &mut self.production
        };
        if !seen.insert(path.clone()) {
            return;
        }
        let ast = syn::parse_file(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let parent = path.parent().unwrap();
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let module_dir = if matches!(stem, "lib" | "main" | "mod") {
            parent.to_owned()
        } else {
            parent.join(stem)
        };
        self.items(&ast.items, &module_dir, parent, gated);
    }
    fn items(&mut self, items: &[syn::Item], module_dir: &Path, path_dir: &Path, gated: bool) {
        for item in items {
            let syn::Item::Mod(module) = item else {
                continue;
            };
            let gated = gated || test_only(&module.attrs);
            let name = module.ident.to_string();
            if let Some((_, items)) = &module.content {
                let dir = module_dir.join(&name);
                self.items(items, &dir, &dir, gated);
                continue;
            }
            let explicit = module.attrs.iter().find_map(|attr| {
                if !attr.path().is_ident("path") {
                    return None;
                }
                let syn::Meta::NameValue(value) = &attr.meta else {
                    panic!("invalid module path");
                };
                let syn::Expr::Lit(value) = &value.value else {
                    panic!("nonliteral module path");
                };
                let syn::Lit::Str(value) = &value.lit else {
                    panic!("nonstring module path");
                };
                Some(path_dir.join(value.value()))
            });
            let path = explicit.unwrap_or_else(|| {
                let flat = module_dir.join(format!("{name}.rs"));
                let nested = module_dir.join(&name).join("mod.rs");
                assert_ne!(
                    flat.exists(),
                    nested.exists(),
                    "missing or ambiguous module {name} in {}",
                    module_dir.display()
                );
                if flat.exists() { flat } else { nested }
            });
            self.load(&path, gated);
        }
    }
}

pub(crate) fn sources_at(root: &Path) -> Vec<Source> {
    fn files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = std::fs::canonicalize(root).unwrap();
    let mut modules = Modules::default();
    for name in ["lib.rs", "main.rs"] {
        let path = root.join(name);
        if path.exists() {
            modules.load(&path, false);
        }
    }
    let mut all = Vec::new();
    files(&root, &mut all);
    all.sort();
    all.into_iter()
        .filter(|path| !modules.tests.contains(path) || modules.production.contains(path))
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
        ("odd.rs", "mod child;"),
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
