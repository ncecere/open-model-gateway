//! Every pooled outbound client sets its pool idle timeout and counts its
//! lost requests under its own label (#483).
//!
//! The wiring is per call site, so a runtime test of the constants cannot
//! see a site that dropped the call or took another client's label. This
//! reads the production source with `syn`: each `client_builder(..)` /
//! `pinned_client_builder(..)` chain must contain
//! `.pool_idle_timeout(UpstreamClient::X.pool_idle_timeout())` with the `X`
//! listed for its file, and each file that maps a send error must count it
//! under the listed client, as many times as it has sites.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use syn::{Expr, visit::Visit};

/// File (below `src/`) and the client its builder chain must name.
const BUILDERS: &[(&str, &str)] = &[
    ("providers/openai.rs", "Openai"),
    ("providers/openrouter.rs", "Openrouter"),
    ("providers/anthropic.rs", "Anthropic"),
    ("providers/bedrock/transport.rs", "Bedrock"),
    ("providers/local/endpoints.rs", "Local"),
    ("filestore/s3/transport.rs", "S3"),
    ("otlp.rs", "Export"),
    // The catalog is openrouter.ai, the same host as the OpenRouter adapter.
    ("management/governance/suggestion.rs", "Openrouter"),
];

/// The one client without a pool idle timeout: identity requests use one
/// connection each (#417), asserted by `pool_max_idle_per_host(0)`.
const SINGLE_CONNECTION: &str = "identity/oidc.rs";

/// File, the client its send-error mappers count under, and how many sites.
const COUNTED: &[(&str, &str, usize)] = &[
    ("providers/openai.rs", "Openai", 1),
    ("providers/openai_responses.rs", "Openai", 1),
    ("providers/openrouter.rs", "Openrouter", 1),
    ("management/governance/suggestion.rs", "Openrouter", 1),
    ("providers/anthropic.rs", "Anthropic", 1),
    ("providers/anthropic/batches.rs", "Anthropic", 2),
    ("providers/local/mod.rs", "Local", 3),
    ("jobs/schedule.rs", "Local", 1),
    ("providers/bedrock/transport.rs", "Bedrock", 1),
    ("filestore/s3/transport.rs", "S3", 1),
    ("otlp.rs", "Export", 2),
    ("alerts/webhooks.rs", "Export", 1),
];

#[derive(Default)]
struct Chain {
    idle: Option<String>,
    single_connection: bool,
}

#[derive(Default)]
struct Found {
    chains: HashMap<usize, Chain>,
    counted: Vec<String>,
}

fn is_cfg_test(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute.meta.require_list().is_ok_and(|list| {
                list.tokens
                    .clone()
                    .into_iter()
                    .any(|t| t.to_string() == "test")
            })
    })
}

/// `X` of a path ending `UpstreamClient::X`.
fn client_of(expr: &Expr) -> Option<String> {
    let Expr::Path(path) = expr else { return None };
    let segments: Vec<_> = path.path.segments.iter().collect();
    match segments.as_slice() {
        [.., owner, name] if owner.ident == "UpstreamClient" => Some(name.ident.to_string()),
        _ => None,
    }
}

fn is_builder(call: &syn::ExprCall) -> bool {
    let Expr::Path(path) = &*call.func else {
        return false;
    };
    path.path.segments.last().is_some_and(|segment| {
        matches!(
            segment.ident.to_string().as_str(),
            "client_builder" | "pinned_client_builder"
        )
    })
}

fn builder_root(mut expr: &Expr) -> Option<&syn::ExprCall> {
    loop {
        match expr {
            Expr::MethodCall(call) => expr = &call.receiver,
            Expr::Call(call) => return is_builder(call).then_some(call),
            _ => return None,
        }
    }
}

impl<'ast> Visit<'ast> for Found {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !is_cfg_test(&node.attrs) {
            syn::visit::visit_item_mod(self, node);
        }
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !is_cfg_test(&node.attrs) {
            syn::visit::visit_item_fn(self, node);
        }
    }
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if !is_cfg_test(&node.attrs) {
            syn::visit::visit_item_impl(self, node);
        }
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if !is_cfg_test(&node.attrs) {
            syn::visit::visit_impl_item_fn(self, node);
        }
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if is_builder(node) {
            self.chains.entry(node as *const _ as usize).or_default();
        }
        if let Expr::Path(path) = &*node.func
            && path
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "send_error")
            && let Some(client) = node.args.first().and_then(client_of)
        {
            self.counted.push(client);
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let method = node.method.to_string();
        if matches!(method.as_str(), "lost_before_response" | "count_then")
            && let Some(client) = client_of(&node.receiver)
        {
            self.counted.push(client);
        }
        if let Some(root) = builder_root(&node.receiver) {
            let chain = self.chains.entry(root as *const _ as usize).or_default();
            match method.as_str() {
                "pool_idle_timeout" => {
                    chain.idle = Some(match node.args.first() {
                        Some(Expr::MethodCall(arg))
                            if arg.method == "pool_idle_timeout" && arg.args.is_empty() =>
                        {
                            client_of(&arg.receiver).unwrap_or_else(|| "?".into())
                        }
                        _ => "?".into(),
                    });
                }
                "pool_max_idle_per_host" => {
                    chain.single_connection = matches!(
                        node.args.first(),
                        Some(Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(n), .. }))
                            if n.base10_digits() == "0"
                    );
                }
                _ => {}
            }
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "tests") {
                sources(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !path
                .file_stem()
                .is_some_and(|stem| stem.to_string_lossy().contains("test"))
        {
            out.push(path);
        }
    }
}

fn scan() -> Vec<(String, Found)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    files
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            let file = syn::parse_file(&text).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            let mut found = Found::default();
            found.visit_file(&file);
            let relative = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (relative, found)
        })
        .collect()
}

#[test]
fn every_client_builder_sets_the_pool_idle_timeout_of_its_client() {
    let mut seen = Vec::new();
    for (file, found) in scan() {
        for chain in found.chains.values() {
            if file == SINGLE_CONNECTION {
                assert!(
                    chain.single_connection && chain.idle.is_none(),
                    "{file}: identity keeps one connection per request (#417)"
                );
                seen.push(file.clone());
                continue;
            }
            let expected = BUILDERS
                .iter()
                .find(|(listed, _)| *listed == file)
                .unwrap_or_else(|| panic!("{file} builds a client: list it in BUILDERS"))
                .1;
            assert_eq!(
                chain.idle.as_deref(),
                Some(expected),
                "{file}: the builder chain must call \
                 .pool_idle_timeout(UpstreamClient::{expected}.pool_idle_timeout())"
            );
            seen.push(file.clone());
        }
    }
    for (file, _) in BUILDERS.iter().chain([(SINGLE_CONNECTION, "")].iter()) {
        assert!(seen.iter().any(|s| s == file), "{file} builds no client");
    }
}

#[test]
fn every_send_error_mapper_counts_under_its_client() {
    for (file, found) in scan() {
        let mut counted = found.counted.clone();
        counted.sort();
        let expected: Vec<String> = COUNTED
            .iter()
            .filter(|(listed, _, _)| *listed == file)
            .flat_map(|(_, client, n)| std::iter::repeat_n((*client).to_owned(), *n))
            .collect();
        assert_eq!(
            counted, expected,
            "{file}: counting sites must match COUNTED (client labels and number of sites)"
        );
    }
}
