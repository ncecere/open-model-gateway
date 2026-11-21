//! `#[dbtest::test]`: a database test that runs against its own copy of a
//! pre-migrated template database.
//!
//! The attribute takes no arguments and wraps one `async fn name(pool: PgPool)`
//! in the same shape `#[sqlx::test]` produces: a plain `#[test] fn name()`
//! whose database is named from `module_path!()::name`, so test names, module
//! paths and `--list` output do not change. The work (template, clone, drop)
//! lives in the invoking crate's `crate::dbtest_runtime::run`; this crate only
//! rewrites the function.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Error, ItemFn};

/// See the crate documentation.
#[proc_macro_attribute]
pub fn test(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = TokenStream2::from(args);
    let result = syn::parse::<ItemFn>(input).and_then(|function| expand(args, function));
    match result {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand(args: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    if !args.is_empty() {
        return Err(Error::new_spanned(
            args,
            "`#[dbtest::test]` takes no arguments: the database always has every enterprise migration applied",
        ));
    }
    if function.sig.asyncness.is_none() {
        return Err(Error::new_spanned(
            function.sig.fn_token,
            "`#[dbtest::test]` requires an `async fn`",
        ));
    }
    if function.sig.inputs.len() != 1 {
        return Err(Error::new_spanned(
            &function.sig.inputs,
            "`#[dbtest::test]` requires exactly one `PgPool` parameter",
        ));
    }
    if !function.sig.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &function.sig.generics,
            "`#[dbtest::test]` functions cannot be generic",
        ));
    }
    let attrs = &function.attrs;
    let name = &function.sig.ident;
    let inputs = &function.sig.inputs;
    let ret = &function.sig.output;
    let body = &function.block;
    Ok(quote! {
        #(#attrs)*
        #[::core::prelude::v1::test]
        fn #name() #ret {
            async fn #name(#inputs) #ret #body

            let f: fn(_) -> _ = #name;
            crate::dbtest_runtime::run(concat!(module_path!(), "::", stringify!(#name)), f)
        }
    })
}
