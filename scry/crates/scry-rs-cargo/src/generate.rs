//! What the generator adds to each source file. All of it goes next to the code it is about, in
//! the same module, because that is the only place private fields and items are reachable from:
//!
//! - **Types**: `#[derive(::scry::Reflect)]` on every struct and enum. The derive's probes make
//!   this safe for any field type: one that cannot be reflected shows as `Debug` text or a name.
//! - **Traits**: hidden `__scry_ref`/`__scry_mut` methods on every trait that can be used as
//!   `dyn Trait`, an override of both in every `impl Trait for T` block of the crate, and
//!   `impl Reflect for dyn Trait` (plus its `Send`/`Sync` variants), so a `Box<dyn Trait>`
//!   reflects the concrete value inside.
//! - **Statics**: a registration next to each non-`mut` static, which `Registry::discovered`
//!   collects at runtime.
//! - **Functions**: a call shim next to each module-level function and each method of a
//!   module-level `impl`; see [`crate::functions`] for which ones qualify.
//! - **Dioxus hooks** (crates that depend on `dioxus`): `let x = use_signal(..)` and the other
//!   state hooks also record their handle under the component's name; see `scry::dioxus_hooks`.
//! - **Endpoint** (executables): started at the top of `main`; in a Dioxus app the components
//!   `main` launches also pump its requests on the UI thread.
//!
//! Anything the generator cannot handle safely is skipped and listed in `scry-report.txt`.

use std::collections::{HashMap, HashSet};

use quote::{ToTokens, format_ident, quote};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, Fields, GenericParam, ImplItemFn, ItemEnum, ItemFn, ItemImpl, ItemMacro, ItemMod, ItemStatic,
    ItemStruct, ItemTrait, ItemUnion, Local, Pat, ReturnType, StaticMutability, TraitBoundModifier, TraitItem,
    TraitItemFn, Type, TypeParamBound, Visibility, WherePredicate,
};

use crate::edits::{Edits, Source};
use crate::functions::{self, Callee};
use crate::tokens;

/// Hooks whose result is a state handle worth tracking.
const STATE_HOOKS: &[&str] = &["use_signal", "use_signal_sync", "use_context_provider", "use_memo"];

const DERIVE: &str = "#[derive(::scry::Reflect)] ";

#[derive(Default)]
pub struct Report {
    pub lines: Vec<String>,
}

impl Report {
    fn generated(&mut self, what: &str, name: &str) {
        self.lines.push(format!("  + {what} {name}"));
    }

    fn skipped(&mut self, what: &str, name: &str, reason: &str) {
        self.lines.push(format!("  - {what} {name}: skipped, {reason}"));
    }
}

pub struct Context<'a> {
    /// Traits, by name, that get `dyn` reflection. Impls are matched by the trait's last path
    /// segment, so a name is only here if every trait of that name in the crate qualifies.
    pub dyn_traits: &'a HashSet<String>,
    /// The crate depends on Dioxus: track hooks, and run the registry on the UI thread.
    pub hooks: bool,
    /// For an executable: start the endpoint in `main`, and for a Dioxus app pump it from these
    /// root components (the ones `main` launches).
    pub endpoint: Option<&'a HashSet<String>>,
}

pub struct Generator<'a> {
    source: &'a Source,
    edits: &'a mut Edits,
    cx: &'a Context<'a>,
    report: &'a mut Report,
    /// The crate root file, where `fn main` is.
    entry: bool,
    inline_mods: usize,
    fns: Vec<String>,
}

impl<'a> Generator<'a> {
    pub fn new(source: &'a Source, edits: &'a mut Edits, cx: &'a Context<'a>, report: &'a mut Report, entry: bool) -> Self {
        Self { source, edits, cx, report, entry, inline_mods: 0, fns: Vec::new() }
    }

    fn start_endpoint(&mut self, item: &ItemFn) {
        let Some(roots) = self.cx.endpoint else { return };
        if self.inline_mods > 0 || !self.fns.is_empty() {
            return;
        }
        let name = item.sig.ident.to_string();
        let call = if self.entry && name == "main" {
            if self.cx.hooks {
                if roots.is_empty() {
                    self.report.skipped("endpoint", "main", "no `launch(Component)` found, so nothing runs the registry");
                }
                "::scry::__private::dioxus::start_endpoint(); "
            } else {
                "::scry::__private::start_endpoint(); "
            }
        } else if self.cx.hooks && roots.contains(&name) {
            "::scry::__private::dioxus::use_endpoint(); "
        } else {
            return;
        };
        self.edits.insert(self.source.end(item.block.brace_token.span.open()), format!(" {call}"));
        self.report.generated("endpoint start in", &name);
    }

    fn derive_at(&mut self, vis: &Visibility, keyword: proc_macro2::Span, what: &str, name: &str) {
        let at = match vis {
            Visibility::Inherited => self.source.start(keyword),
            _ => self.source.start(vis.span()),
        };
        self.edits.insert(at, DERIVE);
        self.report.generated(what, name);
    }
}

impl<'ast> Visit<'ast> for Generator<'_> {
    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        let name = item.ident.to_string();
        let reason = common_skip_reason(&item.attrs, &item.generics).or_else(|| {
            let last = match &item.fields {
                Fields::Named(fields) => fields.named.last(),
                Fields::Unnamed(fields) => fields.unnamed.last(),
                Fields::Unit => None,
            };
            last.filter(|field| is_unsized(&field.ty)).map(|_| "its last field is unsized")
        });
        match reason {
            Some(reason) => self.report.skipped("struct", &name, reason),
            None => self.derive_at(&item.vis, item.struct_token.span, "struct", &name),
        }
        visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast ItemEnum) {
        let name = item.ident.to_string();
        let reason = common_skip_reason(&item.attrs, &item.generics)
            .or_else(|| item.variants.is_empty().then_some("it has no variants"));
        match reason {
            Some(reason) => self.report.skipped("enum", &name, reason),
            None => self.derive_at(&item.vis, item.enum_token.span, "enum", &name),
        }
        visit::visit_item_enum(self, item);
    }

    fn visit_item_union(&mut self, item: &'ast ItemUnion) {
        self.report.skipped("union", &item.ident.to_string(), "unions cannot be read safely");
        visit::visit_item_union(self, item);
    }

    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        let name = item.ident.to_string();
        if self.cx.dyn_traits.contains(&name) {
            self.edits.insert(self.source.end(item.brace_token.span.open()), TRAIT_METHODS);
            self.edits.insert(self.source.end(item.brace_token.span.close()), dyn_impls(item));
            self.report.generated("dyn reflection for trait", &name);
        }
        visit::visit_item_trait(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if self.fns.is_empty() {
            self.impl_shims(item);
        }
        if let Some((None, path, _)) = &item.trait_ {
            let trait_name = path.segments.last().map(|segment| segment.ident.to_string()).unwrap_or_default();
            if self.cx.dyn_traits.contains(&trait_name) {
                let self_ty = item.self_ty.to_token_stream().to_string();
                let label = format!("{trait_name} for {self_ty}");
                if is_unsized(&item.self_ty) || has_maybe_sized(&item.generics) {
                    self.report.skipped("impl", &label, "the type may be unsized; it reflects as its type name");
                } else {
                    self.edits.insert(self.source.end(item.brace_token.span.open()), IMPL_METHODS);
                    self.report.generated("impl", &label);
                }
            }
        }
        visit::visit_item_impl(self, item);
    }

    fn visit_item_static(&mut self, item: &'ast ItemStatic) {
        let name = item.ident.to_string();
        if matches!(item.mutability, StaticMutability::None) {
            let mut path: Vec<&str> = self.fns.iter().map(String::as_str).collect();
            path.push(&name);
            let path = path.join("::");
            self.edits.insert(self.source.end(item.semi_token.span), static_registration(&name, &path));
            self.report.generated("static", &path);
        } else {
            self.report.skipped("static", &name, "a `static mut` cannot be referenced safely");
        }
        visit::visit_item_static(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.start_endpoint(item);
        if self.fns.is_empty() {
            let name = item.sig.ident.to_string();
            let callee = Callee { self_ty: None, trait_path: None, cfgs: functions::cfgs(&item.attrs) };
            match functions::shim(self.source, &item.attrs, &item.sig, &callee) {
                Ok(shim) => {
                    self.edits.insert(self.source.end(item.block.brace_token.span.close()), shim);
                    self.report.generated("fn", &name);
                }
                Err(reason) if name == "main" => drop(reason),
                Err(reason) => self.report.skipped("fn", &name, &reason),
            }
        }
        self.fns.push(item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
        self.fns.pop();
    }

    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        self.inline_mods += 1;
        visit::visit_item_mod(self, item);
        self.inline_mods -= 1;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        self.fns.push(item.sig.ident.to_string());
        visit::visit_impl_item_fn(self, item);
        self.fns.pop();
    }

    fn visit_trait_item_fn(&mut self, item: &'ast TraitItemFn) {
        self.fns.push(item.sig.ident.to_string());
        visit::visit_trait_item_fn(self, item);
        self.fns.pop();
    }

    fn visit_local(&mut self, local: &'ast Local) {
        if self.cx.hooks {
            self.track_hook(local);
        }
        visit::visit_local(self, local);
    }

    fn visit_item_macro(&mut self, item: &'ast ItemMacro) {
        if item.mac.path.is_ident("macro_rules") {
            let name = item.ident.as_ref().map(ToString::to_string).unwrap_or_default();
            if item.attrs.iter().any(|attr| attr.path().is_ident("macro_export")) {
                self.report.skipped("macro", &name, "an exported macro can expand in crates without scry");
            } else {
                let count = tokens::macro_templates(item.mac.tokens.clone(), self.source, self.edits);
                if count > 0 {
                    self.report.generated("types in macro", &format!("{name} ({count})"));
                }
            }
        }
        visit::visit_item_macro(self, item);
    }
}

impl Generator<'_> {
    /// Shims for the functions of a module-level `impl` block, all placed after its closing brace.
    fn impl_shims(&mut self, item: &ItemImpl) {
        let self_name = functions::compact(&item.self_ty);
        let trait_path = match &item.trait_ {
            Some((Some(_), _, _)) => return,
            Some((None, path, _)) => Some(path),
            None => None,
        };
        let label = match trait_path {
            Some(path) => format!("{} for {self_name}", functions::compact(path)),
            None => self_name.clone(),
        };
        let reason = functions::impl_skip_reason(&item.generics, &item.self_ty)
            .map(str::to_owned)
            .or_else(|| item.unsafety.map(|_| "it is an unsafe impl".to_owned()))
            .or_else(|| functions::unknown_attribute(&item.attrs).map(|attribute| format!("its #[{attribute}] attribute may rewrite it")));
        if let Some(reason) = reason {
            if item.items.iter().any(|item| matches!(item, syn::ImplItem::Fn(_))) {
                self.report.skipped("impl", &label, &format!("methods not callable: {reason}"));
            }
            return;
        }
        let mut shims = String::new();
        for impl_item in &item.items {
            let syn::ImplItem::Fn(method) = impl_item else { continue };
            let name = format!("{self_name}::{}", method.sig.ident);
            let callee = Callee {
                self_ty: Some(&item.self_ty),
                trait_path,
                cfgs: functions::cfgs(item.attrs.iter().chain(&method.attrs)),
            };
            match functions::shim(self.source, &method.attrs, &method.sig, &callee) {
                Ok(shim) => {
                    shims.push_str(&shim);
                    self.report.generated("method", &name);
                }
                Err(reason) => self.report.skipped("method", &name, &reason),
            }
        }
        if !shims.is_empty() {
            self.edits.insert(self.source.end(item.brace_token.span.close()), shims);
        }
    }

    fn track_hook(&mut self, local: &Local) {
        let Some(init) = &local.init else { return };
        if init.diverge.is_some() {
            return;
        }
        let Expr::Call(call) = &*init.expr else { return };
        let Expr::Path(func) = &*call.func else { return };
        let Some(hook) = func.path.segments.last().map(|segment| segment.ident.to_string()) else { return };
        if !STATE_HOOKS.contains(&hook.as_str()) {
            return;
        }
        let binding = match &local.pat {
            Pat::Ident(pat) => &pat.ident,
            Pat::Type(pat) => match &*pat.pat {
                Pat::Ident(pat) => &pat.ident,
                _ => return,
            },
            _ => return,
        };
        let component = self.fns.last().map_or("<unknown>", String::as_str);
        let binding = binding.to_string();
        let span = init.expr.span();
        self.edits.insert(self.source.start(span), "{ let __scry_hook = ");
        self.edits.insert(
            self.source.end(span),
            format!(
                "; ::scry::__private::dioxus::track({component:?}, {binding:?}, || {{ \
                 #[allow(unused_imports)] use ::scry::__private::{{ViaDebugOwned as _, ViaOpaqueOwned as _, ViaReflectOwned as _}}; \
                 (&&&::scry::__private::ProbeOwned(&__scry_hook)).scry_owned() }}); __scry_hook }}"
            ),
        );
        self.report.generated("hook", &format!("{component}.{binding} ({hook})"));
    }
}

const TRAIT_METHODS: &str = " #[doc(hidden)] #[allow(dead_code)] \
    fn __scry_ref(&self) -> ::scry::__private::FieldRef<'_> { \
    ::scry::__private::FieldRef::opaque(::core::any::type_name::<Self>()) } \
    #[doc(hidden)] #[allow(dead_code)] \
    fn __scry_mut(&mut self) -> ::scry::__private::FieldMut<'_> { \
    ::scry::__private::FieldMut::Other(::core::any::type_name::<Self>()) } ";

const IMPL_METHODS: &str = " #[allow(clippy::all)] \
    fn __scry_ref(&self) -> ::scry::__private::FieldRef<'_> { \
    #[allow(unused_imports)] use ::scry::__private::{ViaDebug as _, ViaOpaque as _, ViaReflect as _}; \
    (&&&::scry::__private::Probe(self)).scry_ref() } \
    #[allow(clippy::all)] \
    fn __scry_mut(&mut self) -> ::scry::__private::FieldMut<'_> { \
    #[allow(unused_imports)] use ::scry::__private::{ViaOtherMut as _, ViaReflectMut as _}; \
    (&&&::scry::__private::ProbeMut::new(self)).scry_mut() } ";

fn static_registration(name: &str, path: &str) -> String {
    format!(
        " #[::scry::__private::linkme::distributed_slice(::scry::__private::STATICS)] \
         #[linkme(crate = ::scry::__private::linkme)] #[doc(hidden)] #[allow(non_upper_case_globals)] \
         static __SCRY_STATIC_{name}: ::scry::__private::StaticEntry = {{ \
         fn __scry_register(registry: &mut ::scry::Registry) {{ \
         #[allow(unused_imports)] use ::scry::__private::{{ViaDebug as _, ViaOpaque as _, ViaReflect as _}}; \
         registry.__add_field(::core::concat!(::core::module_path!(), \"::\", {path:?}), \
         (&&&::scry::__private::Probe(&{name})).scry_ref()); }} __scry_register }};"
    )
}

/// `impl Reflect for dyn Trait`, for the trait object with each combination of `Send` and `Sync`.
fn dyn_impls(item: &ItemTrait) -> String {
    let name = &item.ident;
    // A trait that must outlive a lifetime (`trait T: 'static`) only forms objects with it.
    let object_lifetime = item.supertraits.iter().find_map(|bound| match bound {
        TypeParamBound::Lifetime(lifetime) => Some(lifetime.clone()),
        _ => None,
    });
    let mut params = Vec::new();
    let lifetime = match &object_lifetime {
        Some(lifetime) => quote!(#lifetime),
        None => {
            params.push(quote!('__scry));
            quote!('__scry)
        }
    };
    let mut args = Vec::new();
    for param in &item.generics.params {
        match param {
            GenericParam::Lifetime(param) => {
                let (lifetime, bounds) = (&param.lifetime, &param.bounds);
                params.push(if bounds.is_empty() { quote!(#lifetime) } else { quote!(#lifetime: #bounds) });
                args.push(quote!(#lifetime));
            }
            GenericParam::Type(param) => {
                let (ident, bounds) = (&param.ident, &param.bounds);
                params.push(if bounds.is_empty() { quote!(#ident) } else { quote!(#ident: #bounds) });
                args.push(quote!(#ident));
            }
            GenericParam::Const(param) => {
                let (ident, ty) = (&param.ident, &param.ty);
                params.push(quote!(const #ident: #ty));
                args.push(quote!(#ident));
            }
        }
    }
    let trait_path = if args.is_empty() { quote!(#name) } else { quote!(#name<#(#args),*>) };
    let mut object_args = args.clone();
    let assoc_types = item.items.iter().filter_map(|item| match item {
        TraitItem::Type(assoc) => Some(assoc),
        _ => None,
    });
    for (index, assoc) in assoc_types.enumerate() {
        let param = format_ident!("__ScryAssoc{}", index);
        let (ident, bounds) = (&assoc.ident, &assoc.bounds);
        params.push(if bounds.is_empty() { quote!(#param) } else { quote!(#param: #bounds) });
        object_args.push(quote!(#ident = #param));
    }
    let object_path = if object_args.is_empty() { quote!(#name) } else { quote!(#name<#(#object_args),*>) };
    let predicates = item.generics.where_clause.as_ref().map(|clause| {
        let predicates = &clause.predicates;
        quote!(#predicates)
    });

    let mut text = String::new();
    for autos in [quote!(), quote!(+ Send), quote!(+ Sync), quote!(+ Send + Sync)] {
        let invocation = quote! {
            ::scry::__private::reflect_dyn! {
                [#(#params),*] [#trait_path] [dyn #object_path #autos + #lifetime] [#predicates]
            }
        };
        text.push(' ');
        text.push_str(&invocation.to_string());
    }
    text
}

fn mentions_reflect(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        (attr.path().is_ident("derive") || attr.path().is_ident("cfg_attr"))
            && attr.to_token_stream().to_string().contains("Reflect")
    })
}

fn is_packed(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("repr") && attr.to_token_stream().to_string().contains("packed"))
}

fn common_skip_reason(attrs: &[Attribute], generics: &syn::Generics) -> Option<&'static str> {
    if mentions_reflect(attrs) {
        Some("it already derives Reflect")
    } else if is_packed(attrs) {
        Some("fields of a packed type cannot be borrowed")
    } else if has_maybe_sized(generics) {
        Some("a type parameter may be unsized")
    } else {
        None
    }
}

fn has_maybe_sized(generics: &syn::Generics) -> bool {
    let maybe = |bounds: &syn::punctuated::Punctuated<TypeParamBound, syn::Token![+]>| {
        bounds.iter().any(|bound| matches!(bound, TypeParamBound::Trait(bound) if matches!(bound.modifier, TraitBoundModifier::Maybe(_))))
    };
    generics.type_params().any(|param| maybe(&param.bounds))
        || generics.where_clause.as_ref().is_some_and(|clause| {
            clause.predicates.iter().any(|predicate| matches!(predicate, WherePredicate::Type(predicate) if maybe(&predicate.bounds)))
        })
}

fn is_unsized(ty: &Type) -> bool {
    match ty {
        Type::Slice(_) | Type::TraitObject(_) => true,
        Type::Path(path) => path.qself.is_none() && path.path.is_ident("str"),
        Type::Paren(inner) => is_unsized(&inner.elem),
        _ => false,
    }
}

/// Collects `mod name;` declarations, for the crate walk and for pointing them at the rewritten
/// files.
pub struct ModDecl {
    pub name: String,
    /// Inline modules (`mod a { mod b; }`) the declaration sits in.
    pub inline_path: Vec<String>,
    pub path_attr: Option<String>,
    /// Where the declaration's own `#[path]` value is, to replace it; or where to insert one.
    pub path_literal: Option<(usize, usize)>,
    pub insert_at: usize,
}

pub struct ModCollector<'a> {
    source: &'a Source,
    inline_path: Vec<String>,
    pub decls: Vec<ModDecl>,
}

impl<'a> ModCollector<'a> {
    pub fn new(source: &'a Source) -> Self {
        Self { source, inline_path: Vec::new(), decls: Vec::new() }
    }
}

impl<'ast> Visit<'ast> for ModCollector<'_> {
    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        let name = item.ident.to_string().trim_start_matches("r#").to_owned();
        if item.content.is_none() {
            let mut path_attr = None;
            let mut path_literal = None;
            for attr in &item.attrs {
                if let syn::Meta::NameValue(meta) = &attr.meta
                    && meta.path.is_ident("path")
                    && let Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(literal), .. }) = &meta.value
                {
                    path_attr = Some(literal.value());
                    path_literal = Some((self.source.start(literal.span()), self.source.end(literal.span())));
                }
            }
            let insert_at = match &item.vis {
                Visibility::Inherited => self.source.start(item.mod_token.span),
                vis => self.source.start(vis.span()),
            };
            self.decls.push(ModDecl { name, inline_path: self.inline_path.clone(), path_attr, path_literal, insert_at });
        } else {
            self.inline_path.push(name);
            visit::visit_item_mod(self, item);
            self.inline_path.pop();
        }
    }
}

/// Whether a trait can be used as `dyn Trait`, conservatively: a trait this accepts must be
/// dyn-compatible, or the generated `impl Reflect for dyn Trait` would not compile. Returns the
/// supertraits that are not known to be fine, which must be traits of this crate that qualify too.
pub fn dyn_candidate(item: &ItemTrait) -> Result<Vec<String>, String> {
    if item.auto_token.is_some() {
        return Err("auto trait".into());
    }
    if item.generics.params.iter().any(|param| matches!(param, GenericParam::Type(param) if param.default.as_ref().is_some_and(mentions_self))) {
        return Err("a type parameter defaults to Self".into());
    }
    if item.generics.where_clause.as_ref().is_some_and(|clause| clause.predicates.iter().any(|p| mentions_self(&p.to_token_stream()))) {
        return Err("its where clause constrains Self".into());
    }
    let mut local_supertraits = Vec::new();
    for bound in &item.supertraits {
        match bound {
            TypeParamBound::Lifetime(_) => {}
            TypeParamBound::Trait(bound) => {
                let Some(last) = bound.path.segments.last() else { continue };
                let name = last.ident.to_string();
                match name.as_str() {
                    "Send" | "Sync" | "Unpin" | "Debug" | "Display" | "Any" | "Error" => {}
                    "Sized" | "Reflect" => return Err(format!("supertrait {name}")),
                    _ if last.arguments.is_empty() => local_supertraits.push(name),
                    _ => return Err(format!("supertrait {name} with arguments")),
                }
            }
            _ => return Err("an unrecognized supertrait bound".into()),
        }
    }
    for trait_item in &item.items {
        match trait_item {
            TraitItem::Const(_) => return Err("it has an associated const".into()),
            TraitItem::Type(assoc) => {
                if !assoc.generics.params.is_empty() {
                    return Err("it has a generic associated type".into());
                }
                if mentions_self(&assoc.bounds.to_token_stream()) {
                    return Err("an associated type's bounds mention Self".into());
                }
            }
            TraitItem::Fn(method) => check_method(method)?,
            TraitItem::Macro(_) => return Err("it has a macro-generated item".into()),
            _ => return Err("it has an unrecognized item".into()),
        }
    }
    Ok(local_supertraits)
}

fn check_method(method: &TraitItemFn) -> Result<(), String> {
    let sig = &method.sig;
    let name = &sig.ident;
    let excluded = sig.generics.where_clause.as_ref().is_some_and(|clause| {
        clause.predicates.iter().any(|predicate| {
            let text = predicate.to_token_stream().to_string();
            text.starts_with("Self :") && text.contains("Sized")
        })
    });
    if excluded {
        return Ok(());
    }
    if sig.asyncness.is_some() {
        return Err(format!("method {name} is async"));
    }
    if sig.receiver().is_none() {
        return Err(format!("method {name} has no self parameter"));
    }
    if sig.generics.params.iter().any(|param| !matches!(param, GenericParam::Lifetime(_))) {
        return Err(format!("method {name} is generic"));
    }
    for input in sig.inputs.iter().skip(1) {
        let tokens = input.to_token_stream();
        if mentions_self(&tokens) || tokens.to_string().contains("impl ") {
            return Err(format!("method {name} takes Self or impl Trait"));
        }
    }
    if let ReturnType::Type(_, ty) = &sig.output {
        let tokens = ty.to_token_stream();
        if mentions_self(&tokens) || matches!(**ty, Type::ImplTrait(_)) {
            return Err(format!("method {name} returns Self or impl Trait"));
        }
    }
    Ok(())
}

/// Whether tokens use `Self` as a type on its own; `Self::Item` is fine in a dyn-compatible trait.
fn mentions_self(tokens: &impl ToTokens) -> bool {
    let tokens: Vec<proc_macro2::TokenTree> = tokens.to_token_stream().into_iter().collect();
    tokens.iter().enumerate().any(|(index, token)| match token {
        proc_macro2::TokenTree::Ident(ident) if ident == "Self" => {
            !matches!(tokens.get(index + 1), Some(proc_macro2::TokenTree::Punct(punct)) if punct.as_char() == ':')
        }
        proc_macro2::TokenTree::Group(group) => mentions_self(&group.stream()),
        _ => false,
    })
}

/// The components `fn main` launches - `dioxus::launch(App)`, `LaunchBuilder::new().launch(App)` -
/// which pump the endpoint's requests on the UI thread.
#[derive(Default)]
pub struct LaunchCollector {
    in_main: bool,
    pub roots: HashSet<String>,
}

impl LaunchCollector {
    fn root(&mut self, args: &syn::punctuated::Punctuated<Expr, syn::Token![,]>) {
        if let Some(Expr::Path(path)) = args.first()
            && let Some(ident) = path.path.get_ident()
        {
            self.roots.insert(ident.to_string());
        }
    }
}

impl<'ast> Visit<'ast> for LaunchCollector {
    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        let was = self.in_main;
        self.in_main = item.sig.ident == "main";
        visit::visit_item_fn(self, item);
        self.in_main = was;
    }

    fn visit_item_mod(&mut self, _item: &'ast ItemMod) {}

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if self.in_main
            && let Expr::Path(func) = &*call.func
            && func.path.segments.last().is_some_and(|segment| segment.ident == "launch")
        {
            self.root(&call.args);
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if self.in_main && call.method == "launch" {
            self.root(&call.args);
        }
        visit::visit_expr_method_call(self, call);
    }
}

/// Every trait defined in a file, with the outcome of [`dyn_candidate`].
#[derive(Default)]
pub struct TraitCollector {
    pub traits: Vec<(String, Result<Vec<String>, String>)>,
}

impl<'ast> Visit<'ast> for TraitCollector {
    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        self.traits.push((item.ident.to_string(), dyn_candidate(item)));
        visit::visit_item_trait(self, item);
    }
}

/// The traits that get `dyn` reflection: candidates whose local supertraits are candidates too,
/// and whose name no rejected trait shares.
pub fn resolve_dyn_traits(traits: &[(String, Result<Vec<String>, String>)], report: &mut Report) -> HashSet<String> {
    let mut supertraits: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut rejected: HashSet<&str> = HashSet::new();
    for (name, outcome) in traits {
        match outcome {
            Ok(local) => supertraits.entry(name).or_default().extend(local.iter().map(String::as_str)),
            Err(reason) => {
                report.skipped("dyn reflection for trait", name, reason);
                rejected.insert(name);
            }
        }
    }
    let mut accepted: HashSet<&str> = supertraits.keys().copied().filter(|name| !rejected.contains(name)).collect();
    loop {
        let failing: Vec<&str> = accepted
            .iter()
            .copied()
            .filter(|name| !supertraits[name].iter().all(|supertrait| accepted.contains(supertrait)))
            .collect();
        if failing.is_empty() {
            break;
        }
        for name in failing {
            accepted.remove(name);
        }
    }
    for name in supertraits.keys().filter(|name| !accepted.contains(*name) && !rejected.contains(*name)) {
        report.skipped("dyn reflection for trait", name, "a supertrait does not qualify");
    }
    accepted.into_iter().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rewrites one file the way the crate walk does, returning the text and the report.
    fn rewrite(text: &str, hooks: bool) -> (String, String) {
        let source = Source::new(text.to_owned());
        let syntax = syn::parse_file(text).unwrap();
        let mut report = Report::default();
        let mut traits = TraitCollector::default();
        traits.visit_file(&syntax);
        let dyn_traits = resolve_dyn_traits(&traits.traits, &mut report);
        let cx = Context { dyn_traits: &dyn_traits, hooks, endpoint: None };
        let mut edits = Edits::default();
        Generator::new(&source, &mut edits, &cx, &mut report, true).visit_file(&syntax);
        let out = edits.apply(text);
        assert_eq!(out.lines().count(), text.lines().count(), "lines moved:\n{out}");
        (out, report.lines.join("\n"))
    }

    #[test]
    fn derives_and_skips_types() {
        let (out, report) = rewrite(
            "pub struct A<T> { t: T }\n#[repr(C, packed)] struct P(u8);\nstruct U { tail: [u8] }\nenum Never {}\n#[derive(scry::Reflect)] struct Done;\n",
            false,
        );
        assert!(out.contains("#[derive(::scry::Reflect)] pub struct A<T>"));
        assert_eq!(out.matches("::scry::Reflect)]").count(), 1, "{out}");
        assert!(report.contains("struct P: skipped"));
        assert!(report.contains("struct U: skipped"));
        assert!(report.contains("enum Never: skipped"));
        assert!(report.contains("struct Done: skipped"));
    }

    #[test]
    fn gives_dyn_reflection_only_to_dyn_compatible_traits() {
        let (out, report) = rewrite(
            "trait Rule: Send { fn apply(&self, x: i64) -> i64; fn make() -> Self where Self: Sized; }\n\
             trait Cloner { fn dup(&self) -> Self; }\n\
             trait Sub: Rule {}\n\
             impl Rule for u8 { fn apply(&self, x: i64) -> i64 { x } }\n\
             impl Cloner for u8 { fn dup(&self) -> Self { *self } }\n\
             impl<T: ?Sized> Sub for Box<T> {}\n",
            false,
        );
        let rule_impls = out.matches("reflect_dyn !").count();
        assert_eq!(rule_impls, 8, "Rule and Sub, four variants each:\n{out}");
        assert!(out.contains("impl Rule for u8 { #[allow(clippy::all)] fn __scry_ref"));
        assert!(!out.contains("impl Cloner for u8 { #[allow"));
        assert!(report.contains("trait Cloner: skipped, method dup returns Self"));
        assert!(report.contains("impl Sub for Box < T >: skipped"));
    }

    #[test]
    fn registers_statics_with_their_function_path() {
        let (out, report) = rewrite("static A: u8 = 1;\nstatic mut B: u8 = 2;\nfn f() { static C: u8 = 3; }\n", false);
        assert!(out.contains("static __SCRY_STATIC_A"));
        assert!(!out.contains("__SCRY_STATIC_B"));
        assert!(out.contains("\"f::C\""));
        assert!(report.contains("static B: skipped"));
    }

    #[test]
    fn tracks_state_hooks_only_with_dioxus() {
        let text = "fn App() { let mut count = use_signal(|| 0);\nlet other = make(); }\n";
        let (out, report) = rewrite(text, true);
        assert!(out.contains("{ let __scry_hook = use_signal(|| 0); ::scry::__private::dioxus::track(\"App\", \"count\""));
        assert!(report.contains("hook App.count (use_signal)"));
        assert_eq!(report.matches("hook").count(), 1);
        assert_eq!(rewrite(text, false).0, text);
    }

    #[test]
    fn emits_call_shims_for_callable_functions_only() {
        let text = "fn add(a: i64, b: &str) -> i64 { a }
#[cfg(windows)] fn only_windows() {}
fn generic<T>(t: T) {}
async fn later() {}
fn takes_mut(v: &mut Vec<u8>) {}
fn takes_impl(v: impl Fn()) {}
#[component] fn card() {}
fn App() {}
#[tokio::main] fn wrapped() {}
fn main() { fn inner() {} }
struct S;
impl S { fn get(&self, key: &'static str) -> u8 { 0 } fn bump(&mut self) {} fn new() -> Self { S } fn eat(self) {} fn same(&self, other: &Self) {} }
impl<T> Wrap<T> { fn unwrap(&self) {} }
impl Default for S { fn default() -> Self { S } }
";
        let (out, report) = rewrite(text, false);
        for callable in ["fn add", "fn only_windows", "method S::get", "method S::bump", "method S::new", "method S::eat", "method S::same", "method S::default"] {
            assert!(report.contains(&format!("+ {callable}")), "{callable} missing:
{report}");
        }
        for (skipped, reason) in [
            ("fn generic", "it is generic"),
            ("fn later", "it is async"),
            ("fn takes_mut", "`&mut` argument"),
            ("fn takes_impl", "`impl Trait` argument"),
            ("fn card", "UI component"),
            ("fn App", "UI component"),
            ("fn wrapped", "#[tokio::main] attribute"),
            ("impl Wrap<T>", "the impl is generic"),
        ] {
            assert!(report.lines().any(|line| line.contains(skipped) && line.contains(reason)), "{skipped}:
{report}");
        }
        assert!(!report.contains("fn main") && !report.contains("inner"), "{report}");

        let shim = |name: &str| out.lines().find(|line| line.contains(&format!("fn {name}"))).unwrap().to_owned();
        assert!(shim("only_windows").contains("} # [cfg (windows)] # [:: scry"), "{}", shim("only_windows"));
        let add = shim("add");
        assert!(add.contains("ArgProbe :: < i64 >") && add.contains("RefProbe :: < str >"), "{add}");
        assert!(add.contains("signature : \"fn add(a: i64, b: &str) -> i64\""), "{add}");
        let methods = shim("get");
        assert!(shim("default").contains("< S as Default > :: default"), "{}", shim("default"));
        assert!(methods.contains("RefProbe :: < S >"), "Self is spelled as the impl type: {methods}");
        assert!(methods.contains("RefProbe :: < str >"), "a named lifetime is dropped: {methods}");
        assert!(methods.contains("SelfProbe (this)"), "{methods}");
        assert!(methods.contains("Receiver :: Mut"), "{methods}");
    }
}
