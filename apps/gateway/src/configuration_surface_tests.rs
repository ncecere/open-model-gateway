//! Regression guard for the audited configuration surface (#197), not a Rust sandbox.
use crate::configuration_source_tests::{production_sources, sources_at, test_only};
use syn::visit::{self, Visit};

const MUTATIONS: &[&str] = &[
    "set_var",
    "remove_var",
    "setenv",
    "unsetenv",
    "putenv",
    "clearenv",
    "_putenv",
    "_wputenv",
    "SetEnvironmentVariableA",
    "SetEnvironmentVariableW",
];
const LOADERS: &[&str] = &[
    "libloading",
    "dlopen2",
    "dlopen",
    "dlmopen",
    "dlsym",
    "LoadLibraryA",
    "LoadLibraryW",
    "LoadLibraryExA",
    "LoadLibraryExW",
    "GetProcAddress",
];
// No production subprocesses exist. CLI commands are Rust subcommands, not OS
// processes. Future exceptions must identify a function and a reviewed reason.
const PROCESS_SPAWN_ALLOWLIST: &[(&str, &str)] = &[];

#[derive(Default)]
struct Guard {
    startup: bool,
    main: bool,
    in_main: bool,
    importing: bool,
    startup_mutations: usize,
    file: String,
    function: String,
    violations: Vec<String>,
}
impl Guard {
    fn check_process(&mut self, path: &str) {
        let path = path.replace("r#", "");
        let process = path == "std::process"
            || path.starts_with("std::process::")
            || path == "tokio::process"
            || path.starts_with("tokio::process::")
            || path == "process::Command"
            || path.starts_with("process::Command::")
            || path.contains("::process::Command")
            || path == "Command::new"
            || path.starts_with("Command::new::");
        if process
            && !PROCESS_SPAWN_ALLOWLIST.iter().any(|(site, reason)| {
                !reason.is_empty() && *site == format!("{}::{}", self.file, self.function)
            })
        {
            self.violations.push(path);
        }
    }
    fn check(&mut self, word: &str) {
        if MUTATIONS.contains(&word) {
            if self.importing {
                self.startup_mutations += 1;
            } else {
                self.violations.push(word.to_owned());
            }
        }
        if LOADERS.contains(&word)
            || (word == "dotenvy" && !self.in_main)
            || (word == "import_secret_files" && !self.importing && !self.in_main)
        {
            self.violations.push(word.to_owned());
        }
    }
    fn macro_tokens(&mut self, tokens: proc_macro2::TokenStream) {
        // Parse item/statement-shaped macro fragments as well, so grouped use
        // aliases cannot hide behind an opaque token group.
        let block = proc_macro2::TokenTree::Group(proc_macro2::Group::new(
            proc_macro2::Delimiter::Brace,
            tokens.clone(),
        ));
        if let Ok(block) = syn::parse2::<syn::Block>(block.into()) {
            self.visit_block(&block);
        }
        let mut compact = String::new();
        for token in tokens {
            match token {
                proc_macro2::TokenTree::Ident(i) => {
                    let name = i.to_string().trim_start_matches("r#").to_owned();
                    self.check(&name);
                    compact.push_str(&name);
                }
                proc_macro2::TokenTree::Punct(p) => compact.push(p.as_char()),
                proc_macro2::TokenTree::Group(g) => {
                    self.macro_tokens(g.stream());
                    compact.push(' ');
                }
                // Text in a string is data, not source tokens.
                proc_macro2::TokenTree::Literal(_) => compact.push(' '),
            }
        }
        for path in [
            "std::process",
            "tokio::process",
            "process::Command",
            "Command::new",
        ] {
            if compact.contains(path) {
                self.check_process(path);
            }
        }
        for alias in [
            "usestdas",
            "usetokioas",
            "usestd::*",
            "usetokio::*",
            "externcratestdas",
            "externcratetokioas",
        ] {
            if compact.contains(alias) {
                self.violations.push(alias.to_owned());
            }
        }
    }
}
impl<'ast> Visit<'ast> for Guard {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !test_only(&node.attrs) {
            visit::visit_item_mod(self, node);
        }
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if test_only(&node.attrs) {
            return;
        }
        let previous = (self.importing, self.in_main, self.function.clone());
        self.function = node.sig.ident.to_string();
        self.importing = self.startup && node.sig.ident == "import_secret_files";
        self.in_main = self.main && node.sig.ident == "main";
        visit::visit_item_fn(self, node);
        (self.importing, self.in_main, self.function) = previous;
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if test_only(&node.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, node.sig.ident.to_string());
        visit::visit_impl_item_fn(self, node);
        self.function = previous;
    }
    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        self.check(ident.to_string().trim_start_matches("r#"));
    }
    fn visit_path(&mut self, node: &'ast syn::Path) {
        self.check_process(
            &node
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::"),
        );
        visit::visit_path(self, node);
    }
    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        fn paths(tree: &syn::UseTree, prefix: &str, out: &mut Vec<String>) {
            match tree {
                syn::UseTree::Path(p) => paths(&p.tree, &format!("{prefix}{}::", p.ident), out),
                syn::UseTree::Name(n) => out.push(format!("{prefix}{}", n.ident)),
                syn::UseTree::Rename(n) => out.push(format!("{prefix}{}", n.ident)),
                syn::UseTree::Glob(_) => out.push(prefix.trim_end_matches("::").to_owned()),
                syn::UseTree::Group(g) => {
                    for tree in &g.items {
                        paths(tree, prefix, out);
                    }
                }
            }
        }
        let mut imports = Vec::new();
        paths(&node.tree, "", &mut imports);
        for import in imports {
            let import = import.replace("r#", "");
            // Root renames, globs and grouped `self as alias` hide the crate
            // name from later paths; ordinary std/tokio imports remain allowed.
            if matches!(
                import.as_str(),
                "std" | "tokio" | "std::self" | "tokio::self"
            ) {
                self.violations.push(import);
            } else {
                self.check_process(&import);
            }
        }
        visit::visit_item_use(self, node);
    }
    fn visit_item_extern_crate(&mut self, node: &'ast syn::ItemExternCrate) {
        if matches!(
            node.ident.to_string().trim_start_matches("r#"),
            "std" | "tokio"
        ) {
            self.violations.push(node.ident.to_string());
        }
        visit::visit_item_extern_crate(self, node);
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.macro_tokens(node.tokens.clone());
        visit::visit_macro(self, node);
    }
}

#[test]
fn production_cannot_mutate_environment_or_load_code() {
    let sources = production_sources();
    assert!(sources.len() > 100, "source scan missed gateway modules");
    let mut mutations = 0;
    for source in sources {
        let mut guard = Guard {
            startup: source.name == "startup.rs",
            main: source.name == "main.rs",
            file: source.name.clone(),
            ..Guard::default()
        };
        guard.visit_file(&source.ast);
        assert!(
            guard.violations.is_empty(),
            "{}: {:?}",
            source.name,
            guard.violations
        );
        mutations += guard.startup_mutations;
    }
    assert_eq!(mutations, 2, "review changes to the startup-only exception");
}

#[test]
fn filenames_alone_never_authorize_test_exclusions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lib.rs"), "mod deceptive_tests;").unwrap();
    std::fs::write(
        dir.path().join("deceptive_tests.rs"),
        "fn bad() { Command::new(input); }",
    )
    .unwrap();
    let source = sources_at(dir.path())
        .into_iter()
        .find(|s| s.name == "deceptive_tests.rs")
        .expect("production despite test-looking name");
    let mut guard = Guard::default();
    guard.visit_file(&source.ast);
    assert!(!guard.violations.is_empty());
    std::fs::write(
        dir.path().join("lib.rs"),
        "#[cfg(test)] mod deceptive_tests;",
    )
    .unwrap();
    assert!(
        !sources_at(dir.path())
            .iter()
            .any(|s| s.name == "deceptive_tests.rs")
    );
}

#[test]
fn source_guard_refuses_process_spawning() {
    for source in [
        "fn run() { std::process::Command::new(input).spawn(); }",
        "use tokio::process::Command as Run; fn run() { Run::new(input).spawn(); }",
        "use std::process as p; fn run() { p::Command::new(input); }",
        "fn run() { launch!(tokio::process::Command::new(input)); }",
        "use std::{process::{Command as Run}}; fn run() { Run::new(input); }",
        "fn run() { Command::new(input); }",
        "fn run() { process::Command::new(input); }",
        "use std as rust; fn run() { rust::process::Command::new(input); }",
        "use tokio as runtime; fn run() { runtime::process::Command::new(input); }",
        "use std::*; fn run() { process::Command::new(input); }",
        "use tokio::*; fn run() { process::Command::new(input); }",
        "use std::{self as rust}; fn run() { rust::process::Command::new(input); }",
        "extern crate std as rust; fn run() { rust::process::Command::new(input); }",
        "extern crate tokio as runtime; fn run() { runtime::process::Command::new(input); }",
        "fn run() { launch!(Command::new(input)); }",
        "fn run() { std::r#process::Command::new(input); }",
        "fn run() { alias!(use std::{self as rust}; let x = 1;); }",
    ] {
        let mut guard = Guard::default();
        guard.visit_file(&syn::parse_file(source).unwrap());
        assert!(
            !guard.violations.is_empty(),
            "process spawning escaped source guard"
        );
    }
}

#[test]
fn source_guard_catches_aliases_and_macros_but_not_test_modules() {
    let source = r#"
        use std::env::set_var as mutate;
        fn change() { libc::putenv(pointer); load!(libloading::Library::new(path)); dotenvy::dotenv(); startup::import_secret_files(); }
        #[cfg(test)] mod tests { fn example() { std::env::remove_var("TEST"); } }
    "#;
    let mut guard = Guard::default();
    guard.visit_file(&syn::parse_file(source).unwrap());
    for word in [
        "set_var",
        "putenv",
        "libloading",
        "dotenvy",
        "import_secret_files",
    ] {
        assert!(guard.violations.iter().any(|v| v == word));
    }
    assert!(!guard.violations.iter().any(|v| v == "remove_var"));
}
