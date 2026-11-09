//! Attribute macros that rewrite the functions they sit on, each the way some real framework's
//! do, so the console demo shows what `cargo scry` makes of functions under macros it has never
//! seen: it tries a call shim and a hook point for each, and leaves out whichever doesn't compile.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{ItemFn, parse_macro_input};

/// Turns the function into a unit struct of the same name, with the body moved to its `handle`
/// method, as a web framework's route macro turns a handler into a service type. The function
/// can no longer be called by name, so its call shim doesn't compile; its body is still a body,
/// so its hook point does.
#[proc_macro_attribute]
pub fn route(_args: TokenStream, item: TokenStream) -> TokenStream {
    let ItemFn { attrs, vis, sig, block } = parse_macro_input!(item as ItemFn);
    let name = &sig.ident;
    let mut handle = sig.clone();
    handle.ident = format_ident!("handle");
    quote! {
        #(#attrs)*
        #[allow(non_camel_case_types)]
        #vis struct #name;

        impl #name {
            #vis #handle #block
        }
    }
    .into()
}

/// Keeps the function, and compiles its body a second time into `<name>_again`, as some macros
/// generate a plain function next to the one they register. Both copies have the hook point, and
/// a hook sees the calls of both.
#[proc_macro_attribute]
pub fn twice(_args: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    let mut again = function.clone();
    again.sig.ident = format_ident!("{}_again", function.sig.ident);
    again.attrs.retain(|attr| attr.path().is_ident("doc"));
    quote!(#function #again).into()
}

/// Rejects a body that declares a `static`, the way a macro rejects code it doesn't expect, with
/// the error at the attribute. A hook point declares statics, so it can't be compiled in here; the
/// function stays callable by name, so its call shim can.
#[proc_macro_attribute]
pub fn plain_body(_args: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    let declares_static = function.block.stmts.iter().any(|stmt| {
        matches!(stmt, syn::Stmt::Item(syn::Item::Static(_)))
            || quote!(#stmt).to_string().split_whitespace().any(|word| word == "static")
    });
    if declares_static {
        return quote!(compile_error!("#[plain_body] takes a body without statics");).into();
    }
    quote!(#function).into()
}
