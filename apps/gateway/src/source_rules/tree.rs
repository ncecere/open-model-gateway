//! The production module tree, read the way the compiler reads it: start at
//! a crate root, follow every `mod x;` (with `#[path]`), drop what a test
//! build alone compiles. Shared by the source rules in this directory.
//!
//! What it keeps: every item whose `cfg` could be true in a build without
//! `cfg(test)` (an unknown predicate such as `feature = "..."` keeps the
//! item, so the rules over-report rather than skip code). What it drops:
//! items under `#[cfg(test)]` (also `all(test, ..)`, and `not(not(test))`),
//! and functions marked `#[test]`, `#[tokio::test]` or `#[sqlx::test]`.
//! Statement-level `cfg` is not pruned (rare; over-reports).
use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::{Attribute, ImplItem, Item, Meta, TraitItem, ext::IdentExt};

/// One parsed production file; `path` is relative to `src/`, with `/`.
pub(super) struct Source {
    pub path: String,
    pub file: syn::File,
    /// Part of the binary crate (`main.rs` and below), not the library: the
    /// library cannot call into it.
    pub bin: bool,
}

pub(super) fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every production file reachable from `entry` (`lib.rs`, `main.rs`).
pub(super) fn production(entry: &str) -> Vec<Source> {
    let mut out = Vec::new();
    load(&src_dir(), Path::new(entry), entry == "main.rs", &mut out);
    out
}

fn load(src: &Path, rel: &Path, bin: bool, out: &mut Vec<Source>) {
    let text =
        std::fs::read_to_string(src.join(rel)).unwrap_or_else(|e| panic!("{}: {e}", rel.display()));
    let mut file = syn::parse_file(&text).unwrap_or_else(|e| panic!("{}: {e}", rel.display()));
    prune(&mut file.items);
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let parent = rel.parent().unwrap_or(Path::new(""));
    let children = if matches!(name, "lib.rs" | "main.rs" | "mod.rs") {
        parent.to_path_buf()
    } else {
        parent.join(rel.file_stem().unwrap_or_default())
    };
    let mut files = Vec::new();
    declared(&file.items, parent, &children, src, rel, &mut files);
    out.push(Source {
        path: rel.to_string_lossy().replace('\\', "/"),
        file,
        bin,
    });
    for child in files {
        load(src, &child, bin, out);
    }
}

/// File modules declared by `items`; `dir` is where a plain `mod x;` looks,
/// `base` is what a `#[path]` is relative to.
fn declared(
    items: &[Item],
    base: &Path,
    dir: &Path,
    src: &Path,
    from: &Path,
    out: &mut Vec<PathBuf>,
) {
    for item in items {
        let Item::Mod(module) = item else { continue };
        let ident = module.ident.unraw().to_string();
        if let Some((_, inner)) = &module.content {
            declared(inner, &base.join(&ident), &dir.join(&ident), src, from, out);
            continue;
        }
        let explicit = module.attrs.iter().find_map(|a| match &a.meta {
            Meta::NameValue(v) if v.path.is_ident("path") => match &v.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(s.value()),
                _ => None,
            },
            _ => None,
        });
        let found = if let Some(path) = explicit {
            Some(base.join(path))
        } else {
            [
                dir.join(format!("{ident}.rs")),
                dir.join(&ident).join("mod.rs"),
            ]
            .into_iter()
            .find(|candidate| src.join(candidate).is_file())
        };
        match found {
            Some(path) if src.join(&path).is_file() => out.push(path),
            _ => panic!(
                "{}: cannot resolve `mod {ident};` (the source rules fail closed)",
                from.display()
            ),
        }
    }
}

/// Evaluates a `cfg` predicate in a build without `cfg(test)`; predicates the
/// scan cannot decide count as true.
fn cfg_holds(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) if path.is_ident("test") => false,
        Meta::List(list) => {
            let name = list
                .path
                .get_ident()
                .map(|i| i.unraw().to_string())
                .unwrap_or_default();
            let args = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
                )
                .map(|a| a.into_iter().collect::<Vec<_>>())
                .unwrap_or_default();
            match name.as_str() {
                "not" => args.first().is_none_or(|m| !cfg_holds(m)),
                "all" => args.iter().all(cfg_holds),
                "any" => args.is_empty() || args.iter().any(cfg_holds),
                _ => true,
            }
        }
        _ => true,
    }
}

fn kept(attrs: &[Attribute]) -> bool {
    attrs.iter().all(|attr| {
        let last = attr
            .path()
            .segments
            .last()
            .map(|s| s.ident.unraw().to_string());
        match (&attr.meta, last.as_deref()) {
            (Meta::List(list), Some("cfg")) => list
                .parse_args::<Meta>()
                .map(|m| cfg_holds(&m))
                .unwrap_or(true),
            (_, Some("test")) => false,
            _ => true,
        }
    })
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn prune(items: &mut Vec<Item>) {
    items.retain(|item| kept(item_attrs(item)));
    for item in items.iter_mut() {
        match item {
            Item::Mod(module) => {
                if let Some((_, inner)) = &mut module.content {
                    prune(inner);
                }
            }
            Item::Impl(imp) => imp.items.retain(|i| {
                kept(match i {
                    ImplItem::Const(c) => &c.attrs,
                    ImplItem::Fn(f) => &f.attrs,
                    ImplItem::Type(t) => &t.attrs,
                    ImplItem::Macro(m) => &m.attrs,
                    _ => &[],
                })
            }),
            Item::Trait(tr) => tr.items.retain(|i| {
                kept(match i {
                    TraitItem::Const(c) => &c.attrs,
                    TraitItem::Fn(f) => &f.attrs,
                    TraitItem::Type(t) => &t.attrs,
                    TraitItem::Macro(m) => &m.attrs,
                    _ => &[],
                })
            }),
            _ => {}
        }
    }
}

/// The enclosing modules, impls and functions of the node being visited.
#[derive(Default)]
pub(super) struct Scope(Vec<String>);

impl Scope {
    pub fn enter(&mut self, name: String) {
        self.0.push(name);
    }
    pub fn leave(&mut self) {
        self.0.pop();
    }
    /// `outer::inner`, or `<file>` outside any named item.
    pub fn path(&self) -> String {
        if self.0.is_empty() {
            "<file>".to_owned()
        } else {
            self.0.join("::")
        }
    }
}

/// The last identifier of an impl's self type (`impl Foo<T>` is `Foo`).
pub(super) fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.unraw().to_string())
            .unwrap_or_default(),
        syn::Type::Reference(r) => type_name(&r.elem),
        _ => "<type>".to_owned(),
    }
}

/// Adds the `visit_*` methods that keep a [`Scope`] in a field called
/// `scope` for a `syn::visit::Visit` impl.
macro_rules! track_scope {
    () => {
        fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.ident).to_string());
            syn::visit::visit_item_mod(self, i);
            self.scope.leave();
        }
        fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
            self.scope.enter(format!(
                "impl {}",
                $crate::source_rules::tree::type_name(&i.self_ty)
            ));
            syn::visit::visit_item_impl(self, i);
            self.scope.leave();
        }
        fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
            self.scope
                .enter(format!("trait {}", syn::ext::IdentExt::unraw(&i.ident)));
            syn::visit::visit_item_trait(self, i);
            self.scope.leave();
        }
        fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.sig.ident).to_string());
            syn::visit::visit_item_fn(self, i);
            self.scope.leave();
        }
        fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.sig.ident).to_string());
            syn::visit::visit_impl_item_fn(self, i);
            self.scope.leave();
        }
        fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
            self.scope
                .enter(syn::ext::IdentExt::unraw(&i.sig.ident).to_string());
            syn::visit::visit_trait_item_fn(self, i);
            self.scope.leave();
        }
    };
}

/// Every token of a macro body, groups flattened, with the token after it.
pub(super) fn flat(tokens: TokenStream) -> Vec<TokenTree> {
    let mut out = Vec::new();
    for token in tokens {
        if let TokenTree::Group(group) = &token {
            let inner = flat(group.stream());
            out.push(token.clone());
            out.extend(inner);
        } else {
            out.push(token);
        }
    }
    out
}

/// Parses `source` as a file and prunes it like [`production`] does; for the
/// planted-violation fixtures.
pub(super) fn parse_fixture(path: &str, source: &str) -> Source {
    let mut file =
        syn::parse_file(source).unwrap_or_else(|e| panic!("fixture {path} does not parse: {e}"));
    prune(&mut file.items);
    let bin = path == "main.rs" || path == "commands.rs" || path.starts_with("commands/");
    Source {
        path: path.to_owned(),
        file,
        bin,
    }
}
