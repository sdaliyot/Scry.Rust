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
//! A hook can also change the call. Before the body, a `hook before` script may write by-value and
//! `&mut` parameters - rebound as `mut` for that - and a `&mut self` receiver, or give a result that
//! replaces running the body; after it, a `hook after` script may replace the result. Values are
//! written through the same probes as `set`, and built through `FromValue` for a replacement.
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

    // What a call captures, and what a `hook before` script may write: by-value and `&mut`
    // parameters (rebound as `mut` so they can be written), and a `&mut self` receiver. A `&T`
    // parameter is never written: that would be writing through a shared reference.
    let mut captures = Vec::new();
    let mut rebinds = Vec::new();
    let mut changes = Vec::new();
    for input in &sig.inputs {
        match input {
            FnArg::Receiver(receiver) => {
                captures.push(quote! {
                    __scry_capture.this(::scry::__private::probe_ref!(&self));
                });
                let mutable = match (&receiver.reference, &*receiver.ty) {
                    (Some(_), _) => receiver.mutability.is_some(),
                    (None, Type::Reference(reference)) => reference.mutability.is_some(),
                    _ => false,
                };
                if mutable {
                    changes.push(quote! {
                        if let ::core::option::Option::Some(__scry_value) = __scry_changes.instance() {
                            __scry_c.change("self", ::scry::__private::probe_mut!(&mut *self), __scry_value);
                        }
                    });
                }
            }
            FnArg::Typed(typed) => {
                let Pat::Ident(pat) = &*typed.pat else { continue };
                if pat.by_ref.is_some() || pat.subpat.is_some() {
                    continue;
                }
                let ident = &pat.ident;
                let name = ident.to_string();
                captures.push(quote! {
                    __scry_capture.arg(#name, ::scry::__private::probe_ref!(&#ident));
                });
                if matches!(&*typed.ty, Type::Reference(reference) if reference.mutability.is_none()) {
                    continue;
                }
                if pat.mutability.is_none() {
                    rebinds.push(quote!(let mut #ident = #ident;));
                }
                changes.push(quote! {
                    if let ::core::option::Option::Some(__scry_value) = __scry_changes.arg(#name) {
                        __scry_c.change(#name, ::scry::__private::probe_mut!(&mut #ident), __scry_value);
                    }
                });
            }
        }
    }

    let returns_type: Type = syn::parse2(returns.clone()).map_err(|_| "its return type cannot be spelled")?;
    let skip_build = crate::functions::build_from_json(quote!(probe_arg), &returns_type, quote!(__scry_value));
    let replace_build = crate::functions::build_from_json(quote!(probe_arg), &returns_type, quote!(&__scry_value));

    let signature = crate::functions::signature_text(source, sig);
    let path = format!("::{label}");
    let allow = quote!(#[allow(unused_mut, clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]);
    // The probe macros bring their own traits.
    let probes = quote!();
    let header = quote! {
        #allow
        static __SCRY_HOOK: ::scry::__private::HookSite =
            ::scry::__private::HookSite::new(::core::concat!(::core::module_path!(), #path), #signature);
        #[::scry::__private::linkme::distributed_slice(::scry::__private::HOOK_SITES)]
        #[linkme(crate = ::scry::__private::linkme)]
        #[allow(non_upper_case_globals, clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]
        static __SCRY_HOOK_SITE: &::scry::__private::HookSite = &__SCRY_HOOK;
        #allow
        let mut __scry_call = __SCRY_HOOK.enter(|__scry_capture| {
            #probes
            #(#captures)*
        });
        #(#allow #rebinds)*
        #allow
        let __scry_skip: ::core::option::Option<#returns> = match &mut __scry_call {
            ::core::option::Option::Some(__scry_c) => match __scry_c.before() {
                ::core::option::Option::Some(__scry_changes) => {
                    #probes
                    #(#changes)*
                    match __scry_changes.result() {
                        ::core::option::Option::Some(__scry_value) => {
                            __scry_c.skip(__scry_value, #skip_build)
                        }
                        ::core::option::Option::None => ::core::option::Option::None,
                    }
                }
                ::core::option::Option::None => ::core::option::Option::None,
            },
            ::core::option::Option::None => ::core::option::Option::None,
        };
    };
    // Lets, so they can carry lint attributes, which an `if` statement cannot.
    let exit = quote! {
        #allow
        let () = if let ::core::option::Option::Some(__scry_c) = &mut __scry_call {
            #probes
            if let ::core::option::Option::Some(__scry_value) = __scry_c.after(::scry::__private::probe_ref!(&__scry_result)) {
                match __scry_c.replace(&__scry_value, #replace_build) {
                    ::core::option::Option::Some(__scry_replacement) => __scry_result = __scry_replacement,
                    ::core::option::Option::None => {
                        __scry_c.change("result", ::scry::__private::probe_mut!(&mut __scry_result), &__scry_value)
                    }
                }
            }
        };
        #allow
        let () = if let ::core::option::Option::Some(__scry_call) = __scry_call {
            #probes
            __scry_call.exit(::scry::__private::probe_ref!(&__scry_result));
        };
    };
    // A function returning `()` ends without the result, so it does not end in a unit expression.
    let tail = if unit { quote!() } else { quote!(__scry_result) };
    let exit = quote!(#exit #tail);
    // The body goes between `open` and `close`, so these are text: a token stream cannot hold an
    // unbalanced brace. The result's type is spelled out: a probe needs it settled, and an
    // `async` block's `?` needs it to know what to convert errors into.
    let (open, close) = if sig.asyncness.is_some() {
        ("async move {".to_owned(), "}.await, };")
    } else {
        (format!("::scry::__private::run_once(move || -> {returns} {{"), "}), };")
    };
    let skipped = "::core::option::Option::Some(__scry_skipped) => __scry_skipped, ::core::option::Option::None =>";
    Ok(HookPoint {
        prefix: format!(" {header} {allow} let mut __scry_result: {returns} = match __scry_skip {{ {skipped} {open} "),
        suffix: format!(" {close} {exit} "),
    })
}

fn mentions_impl_trait(ty: &Type) -> bool {
    ty.to_token_stream()
        .into_iter()
        .any(|token| matches!(&token, proc_macro2::TokenTree::Ident(ident) if ident == "impl"))
        || matches!(ty, Type::ImplTrait(_))
}
