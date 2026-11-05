//! Call shims: a `scry::__private::Function` next to every function and method that can be called
//! from outside, so scripts and `scry call` can run app code.
//!
//! A shim builds each argument from JSON, makes the call, and snapshots the result. It is a
//! `static` in the function's own module, placed after the item on its last line, so private
//! functions and types are reachable and line numbers stay put.
//!
//! What can be called: module-level functions, and the methods and associated functions of
//! module-level `impl` blocks, inherent or trait. What cannot, and why:
//!
//! - generic functions and impls, except for the instantiations the crate itself calls (see
//!   [`crate::generics`]): each of those gets a shim of its own
//! - `unsafe` functions: a call cannot uphold what the caller of one must
//! - methods taking `self` through `Box`/`Rc`/`Arc`; one taking `self` by value is called on a
//!   clone, and refused at run time if the type is not `Clone`
//! - parameters taking `&mut` an unsized type other than `str` or a slice, or `impl Trait` other
//!   than the conversions [`stand_in`] picks a type for (`impl Into<T>`, `impl AsRef<str>`,
//!   `impl IntoIterator<Item = T>`, ...); never closures
//!
//! An `async fn`, or one returning `impl Future`, is run to completion by `scry::__private::block_on`,
//! within the app's tokio runtime when it has one.
//!
//! A `&mut T` parameter gets a `T` built from JSON, and the call's reply then holds the value it
//! was left with: `{"result": .., "args": {"name": ..}}`.
//! - `main`, tests, and Dioxus components (capitalized names, `#[component]`): they only work
//!   when the framework calls them

use std::collections::HashMap;

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::{self, VisitMut};
use syn::{
    Attribute, FnArg, GenericArgument, GenericParam, Lifetime, Path, PathArguments, ReturnType, Signature, Type,
};

use crate::edits::Source;

/// Attributes that leave a function as written. Any other attribute may be a macro that rewrites
/// it (`#[component]` turns a function into one taking a props struct), so the shim would not
/// match.
const INERT_ATTRIBUTES: &[&str] = &[
    "doc",
    "allow",
    "warn",
    "deny",
    "forbid",
    "expect",
    "inline",
    "must_use",
    "cfg",
    "cfg_attr",
    "deprecated",
    "track_caller",
    "cold",
    "no_mangle",
    "export_name",
    "link_section",
    "rustfmt",
    "clippy",
];

/// Attribute macros known to keep a function callable as written, and its body a body: matched by
/// their last path segment, or the whole path where the name alone is too common.
const TRANSPARENT_MACROS: &[&str] = &[
    "instrument",     // tracing
    "async_trait",    // async-trait, on impl blocks and traits
    "cached",         // cached
    "once",           // cached
    "io_cached",      // cached
    "memoize",        // memoize
    "logfn",          // log-derive
    "tauri::command", // tauri: the function stays a plain function next to the command
    "trace",          // tracing-attributes' and others' function tracing
];

/// The first attribute that is not known to leave the item as written. `transparent` adds macros
/// the crate's manifest vouches for (see `crate::config`), matched the same way as
/// [`TRANSPARENT_MACROS`].
pub fn unknown_attribute(attrs: &[Attribute], transparent: &[String]) -> Option<String> {
    attrs
        .iter()
        .map(|attr| attr.path())
        .map(|path| path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>())
        .find(|segments| {
            let full = segments.join("::");
            let last = segments.last().map(String::as_str).unwrap_or_default();
            let known = |name: &str| name == full || name == last;
            !segments.first().is_some_and(|first| INERT_ATTRIBUTES.contains(&first.as_str()))
                && !TRANSPARENT_MACROS.iter().any(|name| known(name))
                && !transparent.iter().any(|name| known(name))
        })
        .map(|segments| segments.join("::"))
}

/// The `#[cfg(..)]` attributes of the function and its impl, which the shim must share.
pub fn cfgs<'a>(attrs: impl IntoIterator<Item = &'a Attribute>) -> TokenStream {
    attrs.into_iter().filter(|attr| attr.path().is_ident("cfg")).map(ToTokens::to_token_stream).collect()
}

/// What the shim is for.
pub struct Callee<'a> {
    /// `Self` of a method or associated function.
    pub self_ty: Option<&'a Type>,
    /// The trait of a trait impl's method.
    pub trait_path: Option<&'a Path>,
    /// `#[cfg]` attributes to repeat on the shim.
    pub cfgs: TokenStream,
    /// Concrete types for the generic parameters of the function or impl being instantiated.
    pub subst: HashMap<String, Type>,
    /// An instantiated generic function's `::<..>`.
    pub turbofish: Option<TokenStream>,
    /// The path to list, when it differs from the written one: `describe::<Item>`,
    /// `Cache<u32, String>`.
    pub label: Option<String>,
    /// Tells apart the shims of one function's instantiations.
    pub variant: usize,
}

impl<'a> Callee<'a> {
    pub fn new(self_ty: Option<&'a Type>, trait_path: Option<&'a Path>, cfgs: TokenStream) -> Self {
        Self { self_ty, trait_path, cfgs, subst: HashMap::new(), turbofish: None, label: None, variant: 0 }
    }
}

/// The `static` to insert after the function, or why there is none.
pub fn shim(
    source: &Source,
    attrs: &[Attribute],
    sig: &Signature,
    callee: &Callee<'_>,
    transparent: &[String],
) -> Result<String, String> {
    let name = sig.ident.to_string();
    if callee.self_ty.is_none() && name == "main" {
        return Err("it is the program's entry point".into());
    }
    if name.starts_with(|c: char| c.is_ascii_uppercase())
        || attrs.iter().any(|attr| attr.path().segments.last().is_some_and(|segment| segment.ident == "component"))
    {
        return Err("it looks like a UI component, which only its framework can call".into());
    }
    if attrs.iter().any(|attr| attr.path().is_ident("test") || attr.path().is_ident("bench")) {
        return Err("it is a test".into());
    }
    if let Some(attribute) = unknown_attribute(attrs, transparent) {
        return Err(format!("its #[{attribute}] attribute may rewrite it; see transparent-attributes"));
    }
    if sig.unsafety.is_some() {
        return Err("it is unsafe".into());
    }
    if sig.variadic.is_some() {
        return Err("it is variadic".into());
    }
    if matches!(&sig.output, ReturnType::Type(_, ty) if matches!(**ty, Type::Never(_))) {
        return Err("it never returns".into());
    }
    let unbound = sig.generics.params.iter().any(|param| match param {
        GenericParam::Lifetime(_) => false,
        GenericParam::Type(param) => !callee.subst.contains_key(&param.ident.to_string()),
        GenericParam::Const(_) => true,
    });
    if unbound {
        return Err("it is generic".into());
    }

    let mut receiver = quote!(::scry::__private::Receiver::None);
    let mut this = TokenStream::new();
    let mut args = Vec::new();
    let mut lets = Vec::new();
    // The `&mut` arguments, by parameter name, reported after the call.
    let mut changed = Vec::new();
    for input in &sig.inputs {
        match input {
            FnArg::Receiver(receiver_arg) => {
                let self_ty = callee.self_ty.ok_or("it takes self outside an impl")?;
                let mutable = match (&receiver_arg.reference, &*receiver_arg.ty) {
                    (Some(_), _) => Some(receiver_arg.mutability.is_some()),
                    (None, Type::Reference(reference)) if is_self(&reference.elem) => {
                        Some(reference.mutability.is_some())
                    }
                    (None, ty) if is_self(ty) => None,
                    _ => return Err("it takes self through a smart pointer".into()),
                };
                if mutable.is_none() {
                    receiver = quote!(::scry::__private::Receiver::Ref);
                    this = quote! {
                        let this = this.downcast_ref::<#self_ty>()?;
                        let this: #self_ty = (&&&::scry::__private::SelfProbe(this)).scry_self()?;
                    };
                } else if mutable == Some(true) {
                    receiver = quote!(::scry::__private::Receiver::Mut);
                    this = quote!(let this = this.downcast_mut::<#self_ty>()?;);
                } else {
                    receiver = quote!(::scry::__private::Receiver::Ref);
                    this = quote!(let this = this.downcast_ref::<#self_ty>()?;);
                }
                args.push(quote!(this));
            }
            FnArg::Typed(typed) => {
                let index = lets.len();
                let binding = quote::format_ident!("arg{index}");
                if let Type::ImplTrait(bounds) = &*typed.ty {
                    if callee.turbofish.is_some() {
                        return Err("it takes `impl Trait` next to generic parameters".into());
                    }
                    let (stand_in, iterate) = stand_in(bounds)?;
                    let ty = spell(&stand_in, callee.self_ty, &callee.subst)?;
                    let build = build_from_json(quote!(probe_arg), &ty, quote!(&args[#index]));
                    lets.push(quote!(let #binding: #ty = #build?;));
                    args.push(if iterate {
                        quote!(::core::iter::IntoIterator::into_iter(#binding))
                    } else {
                        quote!(#binding)
                    });
                    continue;
                }
                let ty = spell(&typed.ty, callee.self_ty, &callee.subst)?;
                match ty {
                    Type::Reference(reference) if reference.mutability.is_some() => {
                        // Built owned, lent mutably, and read back after the call.
                        let elem = &*reference.elem;
                        let (owned, lend): (Type, TokenStream) = match elem {
                            Type::Slice(slice) => {
                                let item = &slice.elem;
                                (syn::parse_quote!(::std::vec::Vec<#item>), quote!(&mut #binding[..]))
                            }
                            Type::Path(path) if path.qself.is_none() && path.path.is_ident("str") => {
                                (syn::parse_quote!(::std::string::String), quote!(#binding.as_mut_str()))
                            }
                            Type::TraitObject(_) => return Err("it takes `&mut dyn Trait`".into()),
                            Type::Path(path)
                                if path.path.segments.last().is_some_and(|last| {
                                    matches!(last.ident.to_string().as_str(), "Path" | "OsStr" | "CStr")
                                }) =>
                            {
                                return Err(format!("it takes `&mut {}`", compact(elem)));
                            }
                            _ => (elem.clone(), quote!(&mut #binding)),
                        };
                        let build = build_from_json(quote!(probe_arg), &owned, quote!(&args[#index]));
                        lets.push(quote!(let mut #binding: #owned = #build?;));
                        args.push(lend);
                        changed.push((param_name(&typed.pat, index), binding));
                    }
                    Type::Reference(reference) => {
                        let elem = &reference.elem;
                        let build = build_from_json(quote!(probe_ref_arg), elem, quote!(&args[#index]));
                        lets.push(quote!(let #binding = #build?;));
                        args.push(quote!(::core::borrow::Borrow::<#elem>::borrow(&#binding)));
                    }
                    ty => {
                        let build = build_from_json(quote!(probe_arg), &ty, quote!(&args[#index]));
                        lets.push(quote!(let #binding: #ty = #build?;));
                        args.push(quote!(#binding));
                    }
                }
            }
        }
    }
    let arity = lets.len();

    let ident = &sig.ident;
    let turbofish = &callee.turbofish;
    let (callee_tokens, path) = match (callee.self_ty, callee.trait_path) {
        (Some(self_ty), Some(trait_path)) => (
            quote!(<#self_ty as #trait_path>::#ident),
            format!(
                "<{} as {}>::{name}",
                callee.label.clone().unwrap_or_else(|| compact(self_ty)),
                compact(trait_path)
            ),
        ),
        (Some(self_ty), None) => (
            quote!(<#self_ty>::#ident),
            format!("{}::{name}", callee.label.clone().unwrap_or_else(|| compact(self_ty))),
        ),
        (None, _) => (quote!(#ident #turbofish), callee.label.clone().unwrap_or_else(|| name.clone())),
    };
    let self_type = match callee.self_ty {
        Some(self_ty) => quote! {{
            fn self_type() -> ::core::any::TypeId { ::core::any::TypeId::of::<#self_ty>() }
            ::core::option::Option::Some(self_type)
        }},
        None => quote!(::core::option::Option::None),
    };
    let trait_name = match callee.trait_path {
        Some(trait_path) => {
            let text = compact(trait_path);
            quote!(::core::option::Option::Some(#text))
        }
        None => quote!(::core::option::Option::None),
    };
    let signature = signature_text(source, sig);
    let path = format!("::{path}");
    let start = sig.ident.span().start();
    let unique = quote::format_ident!("__SCRY_FN_{}_{}_{}", start.line, start.column, callee.variant);

    let awaited = sig.asyncness.is_some()
        || matches!(&sig.output, ReturnType::Type(_, ty) if matches!(&**ty, Type::ImplTrait(bounds)
            if bounds.bounds.iter().any(|bound| matches!(bound, syn::TypeParamBound::Trait(bound)
                if bound.path.segments.last().is_some_and(|last| last.ident == "Future")))));
    let invoke = if awaited {
        quote!(::scry::__private::block_on(#callee_tokens(#(#args),*))?)
    } else {
        quote!(#callee_tokens(#(#args),*))
    };
    // The result is snapshotted, and let go, before the `&mut` arguments are read: it may borrow them.
    let call = if changed.is_empty() {
        quote! {
            let result = #invoke;
            ::core::result::Result::Ok(::scry::__private::probe_ref!(&result).snapshot(depth))
        }
    } else {
        let (names, bindings): (Vec<String>, Vec<_>) = changed.into_iter().unzip();
        quote! {
            let result = {
                let result = #invoke;
                ::scry::__private::probe_ref!(&result).snapshot(depth)
            };
            let mut changed = ::scry::__private::Map::new();
            #(changed.insert(::std::string::String::from(#names), ::scry::__private::probe_ref!(&#bindings).snapshot(depth));)*
            let mut reply = ::scry::__private::Map::new();
            reply.insert(::std::string::String::from("result"), result);
            reply.insert(::std::string::String::from("args"), ::scry::Value::Object(changed));
            ::core::result::Result::Ok(::scry::Value::Object(reply))
        }
    };

    let cfgs = &callee.cfgs;
    let shim = quote! {
        #cfgs
        #[::scry::__private::linkme::distributed_slice(::scry::__private::FUNCTIONS)]
        #[linkme(crate = ::scry::__private::linkme)]
        #[doc(hidden)]
        #[allow(non_upper_case_globals, unused_imports, unused_variables, deprecated, clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]
        static #unique: ::scry::__private::Function = {
            fn invoke(
                this: ::scry::__private::This<'_>,
                args: &[::scry::Value],
                depth: usize,
            ) -> ::core::result::Result<::scry::Value, ::scry::Error> {
                use ::scry::__private::{ViaClone as _, ViaNoClone as _};
                #this
                #(#lets)*
                #call
            }
            ::scry::__private::Function {
                name: #name,
                path: ::core::concat!(::core::module_path!(), #path),
                signature: #signature,
                receiver: #receiver,
                self_type: #self_type,
                trait_name: #trait_name,
                arity: #arity,
                invoke,
            }
        };
    };
    Ok(format!(" {shim}"))
}

/// The type a shim builds for an `impl Trait` parameter, and whether it is passed as an iterator
/// over it: one that is sure to meet every bound, or why there is none.
///
/// - `impl Into<T>`, `impl Borrow<T>` → `T` (`String` for `str`)
/// - `impl AsRef<str>`, `impl ToString`, `impl Display` → `String`; `impl AsRef<Path>` → `PathBuf`;
///   `impl AsRef<OsStr>` → `OsString`; `impl AsRef<[T]>` → `Vec<T>`
/// - `impl IntoIterator<Item = T>` → `Vec<T>`; `impl Iterator<Item = T>` → a `Vec<T>`'s iterator
///
/// Besides one of these, only `Send`, `Sync`, `Unpin`, `Sized` and lifetimes are allowed: any
/// other bound could rule the stand-in out, and the shim would not compile.
pub fn stand_in(bounds: &syn::TypeImplTrait) -> Result<(Type, bool), String> {
    let refuse = || format!("it takes `{}`, which no type is picked for", compact(bounds));
    let mut picked: Option<(Type, bool)> = None;
    for bound in &bounds.bounds {
        let syn::TypeParamBound::Trait(bound) = bound else { continue };
        let Some(last) = bound.path.segments.last() else { return Err(refuse()) };
        let name = last.ident.to_string();
        if matches!(name.as_str(), "Send" | "Sync" | "Unpin" | "Sized") && last.arguments.is_empty() {
            continue;
        }
        let arguments: Vec<&GenericArgument> = match &last.arguments {
            PathArguments::AngleBracketed(arguments) => arguments.args.iter().collect(),
            PathArguments::None => Vec::new(),
            PathArguments::Parenthesized(_) => return Err(refuse()),
        };
        let only_type = match arguments.as_slice() {
            [GenericArgument::Type(ty)] => Some(ty),
            _ => None,
        };
        let item = match arguments.as_slice() {
            [GenericArgument::AssocType(assoc)] if assoc.ident == "Item" => Some(&assoc.ty),
            _ => None,
        };
        let owned = |ty: &Type| -> Option<Type> {
            let last = match ty {
                Type::Path(path) if path.qself.is_none() => {
                    path.path.segments.last().map(|last| last.ident.to_string())
                }
                _ => None,
            };
            match ty {
                _ if last.as_deref() == Some("str") => Some(syn::parse_quote!(::std::string::String)),
                _ if last.as_deref() == Some("Path") => Some(syn::parse_quote!(::std::path::PathBuf)),
                _ if last.as_deref() == Some("OsStr") => Some(syn::parse_quote!(::std::ffi::OsString)),
                Type::Slice(slice) => {
                    let item = &slice.elem;
                    Some(syn::parse_quote!(::std::vec::Vec<#item>))
                }
                _ => None,
            }
        };
        let choice: Option<(Type, bool)> = match (name.as_str(), only_type, item) {
            ("Into" | "Borrow", Some(ty), _) => Some((owned(ty).unwrap_or_else(|| ty.clone()), false)),
            ("AsRef", Some(ty), _) => owned(ty).map(|ty| (ty, false)),
            ("ToString" | "Display", None, None) if arguments.is_empty() => {
                Some((syn::parse_quote!(::std::string::String), false))
            }
            ("IntoIterator", _, Some(item)) => Some((syn::parse_quote!(::std::vec::Vec<#item>), false)),
            ("Iterator", _, Some(item)) => Some((syn::parse_quote!(::std::vec::Vec<#item>), true)),
            _ => None,
        };
        match (choice, &picked) {
            (Some(choice), None) => picked = Some(choice),
            _ => return Err(refuse()),
        }
    }
    picked.ok_or_else(refuse)
}

/// A parameter's name for the reply: its identifier, or `arg<index>` for a pattern.
fn param_name(pat: &syn::Pat, index: usize) -> String {
    match pat {
        syn::Pat::Ident(ident) => ident.ident.to_string(),
        _ => format!("arg{index}"),
    }
}

/// The expression that builds a `ty` from the JSON `value` through `probe` (`probe_arg`,
/// `probe_ref_arg`), or fails. A type that borrows - `&str`, `Option<&T>`, `Cow<'_, str>` - is
/// never built: JSON has nothing to lend it, and a lifetime the shim leaves to inference can make
/// the probes pick a `Deserialize` impl that only holds for one lifetime, which fails to compile.
pub fn build_from_json(probe: TokenStream, ty: &Type, value: TokenStream) -> TokenStream {
    if borrows(ty) {
        let name = compact(ty);
        quote!(::core::result::Result::<_, ::scry::Error>::Err(::scry::Error::CannotConvert { type_name: #name }))
    } else {
        quote!(::scry::__private::#probe!(#ty, #value))
    }
}

/// Whether a type holds a reference or a lifetime other than `'static`.
fn borrows(ty: &Type) -> bool {
    struct Borrows(bool);
    impl<'ast> Visit<'ast> for Borrows {
        fn visit_type_reference(&mut self, _: &'ast syn::TypeReference) {
            self.0 = true;
        }
        fn visit_lifetime(&mut self, lifetime: &'ast Lifetime) {
            self.0 |= lifetime.ident != "static";
        }
    }
    let mut borrows = Borrows(false);
    borrows.visit_type(ty);
    borrows.0
}

/// Whether an `impl` block's methods can get shims: for a type the shim can name as `'static`
/// (`TypeId` needs that). Generic blocks are instantiated by the generator first.
pub fn impl_skip_reason(self_ty: &Type) -> Option<&'static str> {
    if !matches!(self_ty, Type::Path(path) if path.qself.is_none()) {
        return Some("its type is not a plain path");
    }
    let mut lifetimes = Lifetimes(false);
    lifetimes.visit_type(self_ty);
    lifetimes.0.then_some("its type has lifetimes")
}

struct Lifetimes(bool);

impl<'ast> Visit<'ast> for Lifetimes {
    fn visit_lifetime(&mut self, lifetime: &'ast Lifetime) {
        self.0 |= lifetime.ident != "static";
    }
}

fn is_self(ty: &Type) -> bool {
    matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.is_ident("Self"))
}

/// A parameter type as the shim writes it, outside the function: `Self` replaced by the impl's
/// type, and named lifetimes - not in scope there - by `'_`.
pub fn spell(ty: &Type, self_ty: Option<&Type>, subst: &HashMap<String, Type>) -> Result<Type, String> {
    struct Spell<'a> {
        self_ty: Option<&'a Type>,
        subst: &'a HashMap<String, Type>,
        problem: Option<&'static str>,
    }
    impl VisitMut for Spell<'_> {
        fn visit_type_mut(&mut self, ty: &mut Type) {
            match ty {
                Type::ImplTrait(_) => self.problem = Some("it takes an `impl Trait` argument"),
                Type::Macro(_) => self.problem = Some("an argument type is a macro"),
                Type::Infer(_) => self.problem = Some("an argument type is inferred"),
                _ if is_self(ty) => match self.self_ty {
                    Some(self_ty) => *ty = self_ty.clone(),
                    None => self.problem = Some("it mentions Self outside an impl"),
                },
                Type::Path(path)
                    if path.qself.is_none()
                        && let Some(ident) = path.path.get_ident()
                        && let Some(concrete) = self.subst.get(&ident.to_string()) =>
                {
                    *ty = concrete.clone();
                }
                _ => visit_mut::visit_type_mut(self, ty),
            }
        }

        fn visit_path_mut(&mut self, path: &mut Path) {
            if path.segments.first().is_some_and(|segment| segment.ident == "Self") {
                self.problem = Some("an argument type uses an associated type of Self");
            }
            visit_mut::visit_path_mut(self, path);
        }

        fn visit_lifetime_mut(&mut self, lifetime: &mut Lifetime) {
            if lifetime.ident != "static" {
                *lifetime = Lifetime::new("'_", lifetime.span());
            }
        }
    }
    let mut ty = ty.clone();
    let mut spell = Spell { self_ty, subst, problem: None };
    spell.visit_type_mut(&mut ty);
    match spell.problem {
        Some(problem) => Err(problem.into()),
        None => Ok(ty),
    }
}

/// The signature as written, on one line.
pub fn signature_text(source: &Source, sig: &Signature) -> String {
    let end = match &sig.output {
        ReturnType::Type(_, ty) => ty.span(),
        ReturnType::Default => sig.paren_token.span.close(),
    };
    let end = sig.generics.where_clause.as_ref().map_or(end, |clause| clause.span());
    let (start, end) = (source.start(sig.span()), source.end(end));
    let text = source
        .text
        .get(start..end)
        .filter(|_| start < end)
        .map_or_else(|| sig.to_token_stream().to_string(), str::to_owned);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Tokens as compact text: `Cache<u32, String>` rather than `Cache < u32 , String >`.
pub fn compact(tokens: &impl ToTokens) -> String {
    let text = tokens.to_token_stream().to_string();
    let mut out = String::with_capacity(text.len());
    for word in text.split(' ') {
        let previous = out.chars().last();
        let joins = matches!(word, "," | ">" | "<" | "::" | ")" | "(" | "]" | "[")
            || previous.is_some_and(|c| matches!(c, '<' | ':' | '(' | '[' | '&' | ' '));
        if !out.is_empty() && !joins {
            out.push(' ');
        }
        out.push_str(word);
        if word == "," {
            out.push(' ');
        }
    }
    out
}
