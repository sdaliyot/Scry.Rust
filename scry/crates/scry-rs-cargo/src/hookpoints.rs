//! Hook points: the lines around a function's body that let `scry hook add` record its calls.
//!
//! The body stays where it is, line for line. A prefix goes after the opening brace and a suffix
//! before the closing one:
//!
//! ```text
//! fn sell_one(&mut self, index: usize) -> Option<Item> { static SITE ...; let call = SITE.enter(..);
//!     let result = (move || -> Option<Item> {
//!     BODY
//!     })(); call.exit(&result); result }
//! ```
//!
//! The body runs in a `move` closure declared with the function's own return type, so `return` and
//! `?` mean what they meant before, and called as `FnOnce`, so it can return references into its
//! parameters as the function could; an `async fn` uses `async move { BODY }.await` instead, which
//! records the call when its work completes. While no hook is on the function, the cost is one
//! atomic load per call. See `scry::hooks` for what is captured when one is.
//!
//! Which functions get a hook point: module-level functions and the methods of module-level
//! `impl` blocks, generic and `async` ones included. Not these, each listed in the report:
//!
//! - `const fn` (no closures there), `unsafe fn` (its unsafe operations would land in a closure),
//!   `#[track_caller]` (the closure would change what `Location::caller()` sees)
//! - functions returning `!` or `impl Trait`, which a closure cannot be declared to return
//! - a body starting with an inner attribute (`#![allow(..)]`), which must stay first
//! - `main`, tests, UI components and functions under unknown attribute macros, as for calls
//! - functions the crate's `[package.metadata.scry.hooks]` excludes

use quote::{ToTokens, quote};
use syn::{Attribute, FnArg, Pat, ReturnType, Signature, Type};

use crate::edits::Source;

/// The prefix and suffix for a function's body, or why it gets none.
pub struct HookPoint {
    pub prefix: String,
    pub suffix: String,
}

/// `label` is how the function is listed after its module path: `sell_one`,
/// `Inventory::sell_one`, `<Discount as PriceRule>::apply`.
pub fn hook_point(source: &Source, attrs: &[Attribute], sig: &Signature, label: &str) -> Result<HookPoint, String> {
    if sig.constness.is_some() {
        return Err("it is a const fn".into());
    }
    if sig.unsafety.is_some() {
        return Err("it is unsafe".into());
    }
    if sig.variadic.is_some() {
        return Err("it is variadic".into());
    }
    if attrs.iter().any(|attr| attr.path().is_ident("track_caller")) {
        return Err("it is #[track_caller]".into());
    }
    if attrs.iter().any(|attr| matches!(attr.style, syn::AttrStyle::Inner(_))) {
        return Err("its body starts with an inner attribute".into());
    }
    let unit = match &sig.output {
        ReturnType::Default => true,
        ReturnType::Type(_, ty) => matches!(&**ty, Type::Tuple(tuple) if tuple.elems.is_empty()),
    };
    let returns = match &sig.output {
        ReturnType::Default => quote!(()),
        ReturnType::Type(_, ty) if matches!(**ty, Type::Never(_)) => return Err("it returns `!`".into()),
        ReturnType::Type(_, ty) if mentions_impl_trait(ty) => return Err("it returns `impl Trait`".into()),
        ReturnType::Type(_, ty) => ty.to_token_stream(),
    };

    let mut captures = Vec::new();
    for input in &sig.inputs {
        match input {
            FnArg::Receiver(_) => captures.push(quote! {
                __scry_capture.this((&&&::scry::__private::Probe(&self)).scry_ref());
            }),
            FnArg::Typed(typed) => {
                if let Pat::Ident(pat) = &*typed.pat
                    && pat.by_ref.is_none()
                    && pat.subpat.is_none()
                {
                    let ident = &pat.ident;
                    let name = ident.to_string();
                    captures.push(quote! {
                        __scry_capture.arg(#name, (&&&::scry::__private::Probe(&#ident)).scry_ref());
                    });
                }
            }
        }
    }

    let signature = crate::functions::signature_text(source, sig);
    let path = format!("::{label}");
    let header = quote! {
        #[allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]
        static __SCRY_HOOK: ::scry::__private::HookSite =
            ::scry::__private::HookSite::new(::core::concat!(::core::module_path!(), #path), #signature);
        #[::scry::__private::linkme::distributed_slice(::scry::__private::HOOK_SITES)]
        #[linkme(crate = ::scry::__private::linkme)]
        #[allow(non_upper_case_globals, clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]
        static __SCRY_HOOK_SITE: &::scry::__private::HookSite = &__SCRY_HOOK;
        #[allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]
        let __scry_call = __SCRY_HOOK.enter(|__scry_capture| {
            #[allow(unused_imports)]
            use ::scry::__private::{ViaDebug as _, ViaOpaque as _, ViaReflect as _};
            #(#captures)*
        });
    };
    // A `let` so it can carry lint attributes, which an `if` statement cannot.
    let exit = quote! {
        #[allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]
        let () = if let ::core::option::Option::Some(__scry_call) = __scry_call {
            #[allow(unused_imports)]
            use ::scry::__private::{ViaDebug as _, ViaOpaque as _, ViaReflect as _};
            __scry_call.exit((&&&::scry::__private::Probe(&__scry_result)).scry_ref());
        };
    };
    // A function returning `()` ends without the result, so it does not end in a unit expression.
    let tail = if unit { quote!() } else { quote!(__scry_result) };
    let exit = quote!(#exit #tail);
    // The body goes between `open` and `close`, so these are text: a token stream cannot hold an
    // unbalanced brace.
    // The result's type is spelled out: a probe needs it settled, and an `async` block's `?` needs
    // it to know what to convert errors into.
    let (open, close) = if sig.asyncness.is_some() {
        ("async move {".to_owned(), "}.await;")
    } else {
        (format!("::scry::__private::run_once(move || -> {returns} {{"), "});")
    };
    let allow = "#[allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]";
    Ok(HookPoint {
        prefix: format!(" {header} {allow} let __scry_result: {returns} = {open} "),
        suffix: format!(" {close} {exit} "),
    })
}

fn mentions_impl_trait(ty: &Type) -> bool {
    ty.to_token_stream()
        .into_iter()
        .any(|token| matches!(&token, proc_macro2::TokenTree::Ident(ident) if ident == "impl"))
        || matches!(ty, Type::ImplTrait(_))
}
