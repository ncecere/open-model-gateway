//! Secret-like named fields must have no Debug surface until reviewed.
use std::collections::{BTreeMap, BTreeSet};

use quote::ToTokens;
use syn::{
    Attribute, Fields, Ident, Meta, Token, ext::IdentExt, punctuated::Punctuated, visit::Visit,
};

use super::tree::{self, Scope, Source};

/// Resolve explicit import renames and simple type aliases conservatively. A
/// name shared by multiple modules can resolve to any of their targets.
#[derive(Default)]
struct Aliases(BTreeMap<String, BTreeSet<String>>);

impl Aliases {
    fn targets(&self, name: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![name.to_owned()];
        while let Some(name) = pending.pop() {
            if seen.insert(name.clone())
                && let Some(targets) = self.0.get(&name)
            {
                pending.extend(targets.iter().cloned());
            }
        }
        seen
    }

    fn is_debug(&self, path: &syn::Path) -> bool {
        self.targets(&path.segments.last().unwrap().ident.unraw().to_string())
            .contains("Debug")
    }

    fn import(&mut self, tree: &syn::UseTree, parent: Option<&Ident>) {
        use syn::UseTree;
        match tree {
            UseTree::Path(path) => self.import(&path.tree, Some(&path.ident)),
            UseTree::Group(group) => {
                for branch in &group.items {
                    self.import(branch, parent);
                }
            }
            UseTree::Rename(rename) => {
                let original = if rename.ident == "self" {
                    parent.unwrap_or(&rename.ident)
                } else {
                    &rename.ident
                };
                self.0
                    .entry(rename.rename.unraw().to_string())
                    .or_default()
                    .insert(original.unraw().to_string());
            }
            UseTree::Name(_) | UseTree::Glob(_) => {}
        }
    }
}

impl Visit<'_> for Aliases {
    fn visit_item(&mut self, item: &syn::Item) {
        if tree::kept(tree::item_attrs(item)) {
            syn::visit::visit_item(self, item);
        }
    }

    fn visit_item_use(&mut self, item: &syn::ItemUse) {
        self.import(&item.tree, None);
    }

    fn visit_item_type(&mut self, item: &syn::ItemType) {
        self.0
            .entry(item.ident.unraw().to_string())
            .or_default()
            .insert(tree::type_name(&item.ty));
        syn::visit::visit_item_type(self, item);
    }
}

fn derives_debug(meta: &Meta, aliases: &Aliases) -> bool {
    let Meta::List(list) = meta else { return false };
    // The last segment, as `tree::kept` reads attributes: `core::prelude::v1::derive`
    // is the same attribute as `derive`.
    let attr = list.path.segments.last().map(|s| s.ident.unraw());
    if attr.as_ref().is_some_and(|name| name == "derive") {
        return list
            .parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
            .expect("derive paths")
            .iter()
            .any(|p| aliases.is_debug(p));
    }
    if !attr.as_ref().is_some_and(|name| name == "cfg_attr") {
        return false;
    }
    list.parse_args_with(|input: syn::parse::ParseStream| {
        let possible = if input.peek(syn::LitBool) {
            input.parse::<syn::LitBool>()?.value
        } else {
            tree::possible_in_production(&input.parse::<Meta>()?)
        };
        input.parse::<Token![,]>()?;
        let attributes = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        Ok(possible && attributes.iter().any(|attr| derives_debug(attr, aliases)))
    })
    .expect("cfg_attr arguments")
}

#[derive(Default)]
struct Inventory {
    // Match manual impls across files, including qualified and generic self types.
    // With no type resolution, a shared terminal name conservatively matches both.
    fields: BTreeMap<String, Vec<String>>,
    derived: BTreeSet<String>,
    manual: Vec<(String, String)>,
}

struct Scan<'a> {
    file: &'a str,
    aliases: &'a Aliases,
    scope: Scope,
    inventory: &'a mut Inventory,
}

impl Scan<'_> {
    fn fields(
        &mut self,
        name: &Ident,
        variant: Option<&Ident>,
        attrs: &[Attribute],
        fields: &Fields,
    ) {
        let owner = format!("{}::{}::{}", self.file, self.scope.path(), name.unraw());
        for field in fields {
            let Some(ident) = &field.ident else { continue };
            let label = ident.unraw().to_string();
            let sensitive = tree::word_forms(&label).flatten().any(|word| {
                matches!(
                    word.as_str(),
                    "secret"
                        | "secrets"
                        | "token"
                        | "tokens"
                        | "password"
                        | "passwords"
                        | "credential"
                        | "credentials"
                        | "key"
                        | "keys"
                        | "apikey"
                        | "apikeys"
                )
            });
            // The startup importer stores the secret under a neutral name.
            if !(sensitive || owner == "startup.rs::<file>::SecretImport" && label == "value")
                || !tree::kept(&field.attrs)
            {
                continue;
            }
            let variant = variant
                .map(|v| format!("{}::", v.unraw()))
                .unwrap_or_default();
            let entry = format!("{owner}::{variant}{label}: {}", field.ty.to_token_stream());
            if attrs.iter().any(|a| derives_debug(&a.meta, self.aliases)) {
                self.inventory.derived.insert(entry.clone());
            }
            self.inventory
                .fields
                .entry(name.unraw().to_string())
                .or_default()
                .push(entry);
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    track_scope!();

    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !tree::kept(tree::item_attrs(item)) {
            return;
        }
        if let syn::Item::Impl(i) = item
            && let Some((path, _)) = &i.trait_
            && self.aliases.is_debug(path)
        {
            self.inventory.manual.push((
                tree::type_name(&i.self_ty),
                format!(
                    "{}::{}::impl Debug for {}",
                    self.file,
                    self.scope.path(),
                    i.self_ty.to_token_stream()
                ),
            ));
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        self.fields(&item.ident, None, &item.attrs, &item.fields);
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        for variant in &item.variants {
            if tree::kept(&variant.attrs) {
                self.fields(
                    &item.ident,
                    Some(&variant.ident),
                    &item.attrs,
                    &variant.fields,
                );
            }
        }
        syn::visit::visit_item_enum(self, item);
    }
}

fn surfaces(sources: &[Source]) -> BTreeSet<String> {
    let mut aliases = Aliases::default();
    for source in sources {
        aliases.visit_file(&source.file);
    }
    let mut inventory = Inventory::default();
    for source in sources {
        Scan {
            file: &source.path,
            aliases: &aliases,
            scope: Scope::default(),
            inventory: &mut inventory,
        }
        .visit_file(&source.file);
    }
    let mut entries = inventory.derived;
    for (name, location) in inventory.manual {
        for field in aliases
            .targets(&name)
            .iter()
            .filter_map(|n| inventory.fields.get(n))
            .flatten()
        {
            entries.insert(format!("{location} => {field}"));
        }
    }
    entries
}

fn reviewed() -> BTreeSet<String> {
    include_str!("secret_debug.allow")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn production_debug_surfaces_are_reviewed() {
    let mut entries = surfaces(&tree::production("lib.rs"));
    entries.extend(surfaces(&tree::production("main.rs")));
    let allowed = reviewed();
    let unexpected: Vec<_> = entries.difference(&allowed).cloned().collect();
    let stale: Vec<_> = allowed.difference(&entries).cloned().collect();
    assert!(
        unexpected.is_empty() && stale.is_empty(),
        "unreviewed Debug surfaces:\n{}\nstale exceptions:\n{}",
        unexpected.join("\n"),
        stale.join("\n")
    );
}

#[test]
fn planted_derived_and_manual_debug_are_refused() {
    let definition = r#"
        mod formatting;
        #[cfg_attr(feature = "fixture", cfg_attr(unix, derive(core::fmt::Debug)))]
        struct Login { r#password: String }
        enum Entry<T> { Auth { api_key: T, APIkeys: T } }
        #[derive(Debug)] enum Settings { Local { client_secret: String, credentials: String } }
        #[derive(Debug)] struct Other { token_hash: [u8; 32], key_ref: String }
        #[r#cfg_attr(true, r#derive(Debug))] struct Refresh { refresh_token: String }
        #[cfg_attr(false, derive(Debug))] struct Disabled { password: String }
        #[derive(Debug)] struct Keyboard { monkey: String, clientAPIKey: String, clientKEYs: String, APIkey: String }
        #[cfg(test)] #[derive(Debug)] struct TestOnly { token: String }
        #[cfg_attr(test, derive(Debug))] struct OnlyTestDebug { credential: String }
        #[core::prelude::v1::derive(Debug)] struct Pathed { password: String }
    "#;
    let implementation = r#"
        use core::fmt::Debug as Inspect;
        type Stored<T> = crate::Entry<T>;
        impl<T> Inspect for Stored<T> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.write_str("fixture") }
        }
    "#;
    let sources = tree::load_tree(
        &|path| {
            Some(
                if path.ends_with("lib.rs") {
                    definition
                } else {
                    implementation
                }
                .into(),
            )
        },
        "lib.rs",
        false,
    );
    let found = surfaces(&sources);
    assert_eq!(found, BTreeSet::from([
        "lib.rs::<file>::Keyboard::APIkey: String".into(),
        "lib.rs::<file>::Keyboard::clientAPIKey: String".into(),
        "lib.rs::<file>::Keyboard::clientKEYs: String".into(),
        "lib.rs::<file>::Login::password: String".into(),
        "lib.rs::<file>::Pathed::password: String".into(),
        "lib.rs::<file>::Settings::Local::client_secret: String".into(),
        "lib.rs::<file>::Settings::Local::credentials: String".into(),
        "lib.rs::<file>::Refresh::refresh_token: String".into(),
        "lib.rs::<file>::Other::token_hash: [u8 ; 32]".into(),
        "lib.rs::<file>::Other::key_ref: String".into(),
        "formatting.rs::<file>::impl Debug for Stored < T > => lib.rs::<file>::Entry::Auth::api_key: T".into(),
        "formatting.rs::<file>::impl Debug for Stored < T > => lib.rs::<file>::Entry::Auth::APIkeys: T".into(),
    ]));
}

#[test]
fn grouped_imports_and_transparent_self_types_preserve_manual_review() {
    let source = tree::parse_fixture(
        "lib.rs",
        r#"
        enum Session<T> { Open { token: T } }
        use std::fmt::Debug::{self as Show};
        use crate::Session::{self as Imported};
        type Local<T> = (Imported<T>);
        impl<T> Show for (Local<T>) {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.write_str("session") }
        }
        fn unrelated() {
            #[cfg(test)] #[derive(Debug)] struct TestOnly { api_key: String }
        }
    "#,
    );
    assert_eq!(
        surfaces(&[source]),
        BTreeSet::from([
            "lib.rs::<file>::impl Debug for (Local < T >) => lib.rs::<file>::Session::Open::token: T"
                .into(),
        ])
    );
}
