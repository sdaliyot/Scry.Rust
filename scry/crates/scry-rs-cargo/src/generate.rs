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
//!   module-level `impl`; see [`crate::functions`] for which ones qualify. Generic ones get one
//!   per instantiation the crate calls, found in its MIR; see [`crate::generics`].
//! - **Dioxus hooks** (crates that depend on `dioxus`): `let x = use_signal(..)` and the other
//!   state hooks also record their handle under the component's name; see `scry::dioxus_hooks`.
//! - **Hook points**: a few lines around the body of each hookable function, so `scry-rs hook add`
//!   can record its calls; see [`crate::hookpoints`].
//! - **Endpoint** (executables): started at the top of `main`; in a Dioxus app the components
//!   `main` launches also pump its requests on the UI thread.
//!
//! Anything the generator cannot handle safely is skipped and listed in `scry-report.txt`.

use std::collections::{HashMap, HashSet};

use quote::{ToTokens, format_ident, quote};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, Fields, GenericParam, ImplItemFn, ItemConst, ItemEnum, ItemFn, ItemImpl, ItemMacro, ItemMod,
    ItemStatic, ItemStruct, ItemTrait, ItemUnion, Local, Pat, ReturnType, StaticMutability, TraitBoundModifier,
    TraitItem, TraitItemFn, Type, TypeParamBound, Visibility, WherePredicate,
};

use crate::cfg::Cfg;
use crate::config::Config;
use crate::edits::{Edits, Source};
use crate::functions::{self, Callee};
use crate::generics::{self, Index, Use};
use crate::hookpoints;
use crate::pieces::{Dropped, Part, Piece};
use crate::tokens;

/// Hooks whose result is a state handle worth tracking.
const STATE_HOOKS: &[&str] = &[
    "use_signal",
    "use_signal_sync",
    "use_context_provider",
    "use_root_context",
    "use_memo",
    "use_resource",
    "use_action",
];

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

/// Why a generic function or impl is skipped when the search for instantiations is turned off.
const GENERICS_OFF: &str = "generic, and generics are off by [package.metadata.scry.generics]";

pub struct Context<'a> {
    /// Traits, by name, that get `dyn` reflection. Impls are matched by the trait's last path
    /// segment, so a name is only here if every trait of that name in the crate qualifies.
    pub dyn_traits: &'a HashSet<String>,
    /// The crate depends on Dioxus: track hooks, and run the registry on the UI thread.
    pub hooks: bool,
    /// For an executable: start the endpoint in `main`, and for a Dioxus app pump it from these
    /// root components (the ones `main` launches).
    pub endpoint: Option<&'a HashSet<String>>,
    /// The crate's types and modules, to name generic arguments from a shim.
    pub index: &'a Index,
    /// The concrete uses of the crate's generic functions and types, by name.
    pub uses: &'a HashMap<String, HashSet<Use>>,
    /// Uses were looked for: off by `[package.metadata.scry.generics]`, `uses` is empty for that.
    pub generics: bool,
    pub crate_name: &'a str,
    /// Hook points are compiled in, with these exclusions; `None` when they are off.
    pub hook_points: Option<&'a Config>,
    /// Call shims and hook points left out, because they did not compile (see `crate::pieces`).
    pub dropped: &'a Dropped,
}

pub struct Generator<'a> {
    source: &'a Source,
    edits: &'a mut Edits,
    cx: &'a Context<'a>,
    report: &'a mut Report,
    /// The crate root file, where `fn main` is.
    entry: bool,
    inline_mods: usize,
    /// The module being visited, from the crate root.
    module: Vec<String>,
    fns: Vec<String>,
    /// How many hook points this file got, for the report.
    pub hook_points: usize,
}

impl<'a> Generator<'a> {
    pub fn new(
        source: &'a Source,
        edits: &'a mut Edits,
        cx: &'a Context<'a>,
        report: &'a mut Report,
        entry: bool,
        module: Vec<String>,
    ) -> Self {
        Self { source, edits, cx, report, entry, inline_mods: 0, module, fns: Vec::new(), hook_points: 0 }
    }

    fn start_endpoint(&mut self, item: &ItemFn) {
        let Some(roots) = self.cx.endpoint else { return };
        if self.inline_mods > 0 || !self.fns.is_empty() {
            return;
        }
        let name = item.sig.ident.to_string();
        let call = if self.entry && name == "main" {
            if self.cx.hooks && !roots.is_empty() {
                "::scry::__private::dioxus::start_endpoint(); "
            } else {
                if self.cx.hooks {
                    // A server built from the same crate as its Dioxus client, say.
                    self.report.skipped(
                        "Dioxus endpoint",
                        "main",
                        "this build launches no `launch(Component)`, so the registry runs on a thread of its own",
                    );
                }
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
            self.impl_hook_points(item);
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
            self.edits.insert(
                self.source.end(item.semi_token.span),
                static_registration(
                    &name,
                    &path,
                    &item.ty.to_token_stream().to_string(),
                    &functions::cfgs(&item.attrs).to_string(),
                ),
            );
            self.report.generated("static", &path);
        } else {
            self.report.skipped("static", &name, "a `static mut` cannot be referenced safely");
        }
        visit::visit_item_static(self, item);
    }

    fn visit_item_const(&mut self, item: &'ast ItemConst) {
        let name = item.ident.to_string();
        let mut path: Vec<&str> = self.fns.iter().map(String::as_str).collect();
        path.push(&name);
        let path = path.join("::");
        if name == "_" {
            // `const _: () = ...;`, a compile-time check with nothing to read.
        } else if !item.generics.params.is_empty() {
            self.report.skipped("const", &path, "it is generic");
        } else {
            self.edits.insert(
                self.source.end(item.semi_token.span),
                const_registration(
                    &name,
                    &path,
                    &item.ty.to_token_stream().to_string(),
                    &functions::cfgs(&item.attrs).to_string(),
                ),
            );
            self.report.generated("const", &path);
        }
        visit::visit_item_const(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.start_endpoint(item);
        if self.fns.is_empty() {
            self.fn_shims(item);
            let name = item.sig.ident.to_string();
            if name != "main" {
                self.hook_point(&item.attrs, &item.sig, &item.block, &name, std::slice::from_ref(&name), None);
            }
        }
        self.fns.push(item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
        self.fns.pop();
    }

    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        self.inline_mods += 1;
        self.module.push(item.ident.to_string());
        visit::visit_item_mod(self, item);
        self.module.pop();
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
    /// The shim for a module-level function, or one per concrete use of a generic one.
    fn fn_shims(&mut self, item: &ItemFn) {
        let name = item.sig.ident.to_string();
        let piece = Piece::new(&self.module, &name, Part::Shim);
        if let Some(reason) = self.dropped(&item.attrs, &piece) {
            self.report.skipped("fn", &name, &reason);
            return;
        }
        let under = self.under(&item.attrs);
        let cfgs = functions::cfgs(&item.attrs);
        let params: Vec<String> = item.sig.generics.type_params().map(|param| param.ident.to_string()).collect();
        if params.is_empty() || item.sig.generics.const_params().next().is_some() {
            match functions::shim(self.source, &item.attrs, &item.sig, &Callee::new(None, None, cfgs)) {
                Ok(shim) => {
                    self.edits.insert_piece(self.source.end(item.block.brace_token.span.close()), shim, piece);
                    self.report.generated("fn", &format!("{name}{under}"));
                }
                Err(_) if name == "main" => {}
                Err(reason) => self.report.skipped("fn", &name, &reason),
            }
            return;
        }

        let mut uses: Vec<&Use> = self
            .cx
            .uses
            .get(&name)
            .into_iter()
            .flatten()
            .filter(|use_| {
                use_.method.is_none()
                    && generics::qualifies(&use_.qualifier, &self.module, self.cx.crate_name)
                    && !self.inside_generic_code(&type_args(&use_.args))
            })
            .collect();
        uses.sort_by(|a, b| a.args.cmp(&b.args));
        uses.dedup_by(|a, b| a.args == b.args);
        let mut shims = String::new();
        let mut problems = Vec::new();
        for (variant, use_) in uses.iter().enumerate() {
            let args = type_args(&use_.args);
            let resolved = match self.resolve_all(&args, params.len()) {
                Ok(resolved) => resolved,
                Err(problem) => {
                    problems.push(problem);
                    continue;
                }
            };
            let label = format!("{name}::<{}>", args.join(", "));
            let mut callee = Callee::new(None, None, cfgs.clone());
            callee.subst = params.iter().cloned().zip(resolved.iter().cloned()).collect();
            callee.turbofish = Some(quote!(::<#(#resolved),*>));
            callee.label = Some(label.clone());
            callee.variant = variant;
            match functions::shim(self.source, &item.attrs, &item.sig, &callee) {
                Ok(shim) => {
                    shims.push_str(&shim);
                    self.report.generated("fn", &format!("{label}{under}"));
                }
                Err(reason) => problems.push(reason),
            }
        }
        if !shims.is_empty() {
            self.edits.insert_piece(self.source.end(item.block.brace_token.span.close()), shims, piece);
        }
        if let Some(problem) = problems.first() {
            let reason = format!("generic, and not every instantiation can be called: {problem}");
            self.report.skipped("fn", &name, &reason);
        } else if !self.cx.generics {
            self.report.skipped("fn", &name, GENERICS_OFF);
        } else if uses.is_empty() {
            self.report.skipped("fn", &name, "generic, and the crate never calls it with concrete types");
        }
    }

    /// Whether MIR's arguments come from inside generic code - a bare name such as `T` that is no
    /// type the crate or std defines - rather than from a concrete call.
    fn inside_generic_code(&self, args: &[String]) -> bool {
        args.iter().any(|arg| {
            arg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && self.cx.index.resolve(arg, &self.module, self.cx.crate_name).is_err()
        })
    }

    /// MIR's type arguments resolved from this module, or the first that cannot be.
    fn resolve_all(&self, args: &[String], expected: usize) -> Result<Vec<Type>, String> {
        if args.len() != expected {
            return Err(format!("expected {expected} type arguments, MIR has {}", args.len()));
        }
        args.iter().map(|arg| self.cx.index.resolve(arg, &self.module, self.cx.crate_name)).collect()
    }

    /// The instantiations of a generic `impl` block to give shims: each list of type arguments for
    /// `Self` that the crate calls one of the block's methods with - which proves the block's
    /// bounds hold - with the methods called that way. Returns the impl's parameters in the order
    /// `Self` takes them.
    fn impl_instantiations(&self, item: &ItemImpl) -> Result<(Vec<String>, Vec<Instance>), String> {
        if item.generics.lifetimes().next().is_some() || item.generics.const_params().next().is_some() {
            return Err("the impl has lifetime or const parameters".into());
        }
        let shape = "the impl is for a specific shape of its type";
        let Type::Path(self_path) = &*item.self_ty else { return Err("its type is not a plain path".into()) };
        let last = self_path.path.segments.last().ok_or("its type has no name")?;
        let syn::PathArguments::AngleBracketed(written) = &last.arguments else { return Err(shape.into()) };
        // `impl<K, V> Cache<K, V>`: each argument must be one of the impl's parameters, once.
        let mut params = Vec::new();
        for arg in &written.args {
            match arg {
                syn::GenericArgument::Type(Type::Path(path)) if path.qself.is_none() => match path.path.get_ident() {
                    Some(ident) => params.push(ident.to_string()),
                    None => return Err(shape.into()),
                },
                _ => return Err(shape.into()),
            }
        }
        let declared: HashSet<String> = item.generics.type_params().map(|param| param.ident.to_string()).collect();
        let written_set: HashSet<String> = params.iter().cloned().collect();
        if written_set.len() != params.len() || written_set != declared {
            return Err(shape.into());
        }

        let type_name = last.ident.to_string();
        let type_module = self.cx.index.module_of(&type_name, &self.module).unwrap_or_else(|| self.module.clone());
        let methods: HashSet<String> = item
            .items
            .iter()
            .filter_map(|item| match item {
                syn::ImplItem::Fn(method) => Some(method.sig.ident.to_string()),
                _ => None,
            })
            .collect();
        let mut found: Vec<Instance> = Vec::new();
        for use_ in self.cx.uses.get(&type_name).into_iter().flatten() {
            let Some(method) = &use_.method else { continue };
            if !methods.contains(method) || !generics::qualifies(&use_.qualifier, &type_module, self.cx.crate_name) {
                continue;
            }
            let args = type_args(&use_.args);
            if self.inside_generic_code(&args) {
                continue;
            }
            match found.iter_mut().find(|instance| instance.args == args) {
                Some(instance) => {
                    instance.called.insert(method.clone());
                }
                None => found.push(Instance { args, called: HashSet::from([method.clone()]) }),
            }
        }
        found.sort_by(|a, b| a.args.cmp(&b.args));
        Ok((params, found))
    }

    /// Shims for the functions of a module-level `impl` block, all placed after its closing brace:
    /// once for a plain block, once per instantiation for a generic one.
    fn impl_shims(&mut self, item: &ItemImpl) {
        if matches!(&item.trait_, Some((Some(_), _, _))) {
            return;
        }
        let label = match &item.trait_ {
            Some((_, path, _)) => format!("{} for {}", functions::compact(path), functions::compact(&item.self_ty)),
            None => functions::compact(&item.self_ty),
        };
        let has_methods = item.items.iter().any(|item| matches!(item, syn::ImplItem::Fn(_)));
        let reason = functions::impl_skip_reason(&item.self_ty)
            .map(str::to_owned)
            .or_else(|| item.unsafety.map(|_| "it is an unsafe impl".to_owned()));
        if let Some(reason) = reason {
            if has_methods {
                self.report.skipped("impl", &label, &format!("methods not callable: {reason}"));
            }
            return;
        }
        if item.generics.params.is_empty() {
            let shims = self.impl_block_shims(item, &HashMap::new(), None);
            self.insert_shims(item, shims);
            return;
        }

        let (params, found) = match self.impl_instantiations(item) {
            Ok(result) => result,
            Err(reason) => {
                if has_methods {
                    self.report.skipped("impl", &label, &format!("methods not callable: {reason}"));
                }
                return;
            }
        };
        if found.is_empty() && has_methods {
            let reason = if self.cx.generics {
                "generic, and the crate never calls its methods with concrete types"
            } else {
                GENERICS_OFF
            };
            self.report.skipped("impl", &label, reason);
        }
        let mut shims = Vec::new();
        for (variant, instance) in found.iter().enumerate() {
            match self.resolve_all(&instance.args, params.len()) {
                Ok(resolved) => {
                    let subst: HashMap<String, Type> = params.iter().cloned().zip(resolved).collect();
                    shims.extend(self.impl_block_shims(item, &subst, Some((instance, variant))));
                }
                Err(problem) => {
                    let label = format!("{label} with <{}>", instance.args.join(", "));
                    self.report.skipped("impl", &label, &problem);
                }
            }
        }
        self.insert_shims(item, shims);
    }

    /// Each method's shims after the impl block, each a piece of its own. When an attribute macro
    /// on the block may report its errors there, the block is marked as part of each.
    fn insert_shims(&mut self, item: &ItemImpl, shims: Vec<(String, Piece)>) {
        let block = functions::unknown_attribute(&item.attrs)
            .map(|_| (self.source.start(item.span()), self.source.end(item.brace_token.span.close())));
        for (shim, piece) in shims {
            if let Some((start, end)) = block {
                self.edits.mark(start, end, piece.clone());
            }
            self.edits.insert_piece(self.source.end(item.brace_token.span.close()), shim, piece);
        }
    }

    /// The shims for the methods of one impl block, with the type arguments in `subst` for a
    /// generic one.
    fn impl_block_shims(
        &mut self,
        item: &ItemImpl,
        subst: &HashMap<String, Type>,
        instance: Option<(&Instance, usize)>,
    ) -> Vec<(String, Piece)> {
        let concrete_self = match functions::spell(&item.self_ty, None, subst) {
            Ok(ty) => ty,
            Err(reason) => {
                self.report.skipped("impl", &functions::compact(&item.self_ty), &reason);
                return Vec::new();
            }
        };
        let concrete_trait = match &item.trait_ {
            Some((_, path, _)) => {
                let as_type: Type = syn::parse_quote!(#path);
                match functions::spell(&as_type, None, subst) {
                    Ok(Type::Path(path)) => Some(path.path),
                    _ => return Vec::new(),
                }
            }
            None => None,
        };
        // How the instantiation is listed: as MIR printed it, `Cache<u32, String>`.
        let self_label = instance.map(|(instance, _)| {
            let base = functions::compact(&item.self_ty);
            format!("{}<{}>", base.split('<').next().unwrap_or(&base), instance.args.join(", "))
        });
        let self_name = self_label.clone().unwrap_or_else(|| functions::compact(&item.self_ty));
        let base_name = functions::compact(&item.self_ty);
        let base_name = base_name.split('<').next().unwrap_or(&base_name).to_owned();
        let mut shims = Vec::new();
        for impl_item in &item.items {
            let syn::ImplItem::Fn(method) = impl_item else { continue };
            let name = format!("{self_name}::{}", method.sig.ident);
            let piece = Piece::new(&self.module, &format!("{base_name}::{}", method.sig.ident), Part::Shim);
            let attrs: Vec<Attribute> = item.attrs.iter().chain(&method.attrs).cloned().collect();
            if let Some(reason) = self.dropped(&attrs, &piece) {
                self.report.skipped("method", &name, &reason);
                continue;
            }
            let under = self.under(&attrs);
            if let Some((instance, _)) = instance
                && method.sig.generics.where_clause.is_some()
                && !instance.called.contains(&method.sig.ident.to_string())
            {
                self.report.skipped("method", &name, "it has bounds of its own, and is not called with these types");
                continue;
            }
            let mut callee = Callee::new(
                Some(&concrete_self),
                concrete_trait.as_ref(),
                functions::cfgs(item.attrs.iter().chain(&method.attrs)),
            );
            callee.subst = subst.clone();
            callee.label = self_label.clone();
            callee.variant = instance.map_or(0, |(_, variant)| variant);
            match functions::shim(self.source, &method.attrs, &method.sig, &callee) {
                Ok(shim) => {
                    shims.push((shim, piece));
                    self.report.generated("method", &format!("{name}{under}"));
                }
                Err(reason) => self.report.skipped("method", &name, &reason),
            }
        }
        shims
    }

    /// Why `piece` is left out, if it is: it did not compile in an earlier attempt.
    fn dropped(&self, attrs: &[Attribute], piece: &Piece) -> Option<String> {
        if !self.cx.dropped.contains(piece) {
            return None;
        }
        let what = match piece.part {
            Part::Shim => "its call shim",
            Part::HookPoint => "its hook point",
        };
        Some(match functions::unknown_attribute(attrs) {
            Some(attribute) => format!("{what} did not compile under its #[{attribute}] attribute, which rewrites it"),
            None => format!("{what} did not compile"),
        })
    }

    /// For the report: ` (under #[attr])` for code under an attribute macro not known to keep it.
    fn under(&self, attrs: &[Attribute]) -> String {
        functions::unknown_attribute(attrs).map(|attribute| format!(" (under #[{attribute}])")).unwrap_or_default()
    }

    /// A hook point in a function's body, unless hooks are off, the function is excluded, or it
    /// cannot take one. `label` is its path after the module; `segments` the same as path segments,
    /// for the exclusion patterns.
    fn hook_point(
        &mut self,
        attrs: &[Attribute],
        sig: &syn::Signature,
        block: &syn::Block,
        label: &str,
        segments: &[String],
        outer: Option<(usize, usize)>,
    ) {
        let Some(config) = self.cx.hook_points else { return };
        let mut path = vec!["crate".to_owned()];
        path.extend(self.module.iter().cloned());
        path.extend(segments.iter().cloned());
        if config.hook_excluded(&path) {
            self.report.skipped("hook point", label, "excluded by [package.metadata.scry.hooks]");
            return;
        }
        let name = sig.ident.to_string();
        let reason = if name.starts_with(|c: char| c.is_ascii_uppercase())
            || attrs.iter().any(|attr| attr.path().segments.last().is_some_and(|segment| segment.ident == "component"))
        {
            Some("it looks like a UI component".to_owned())
        } else if attrs.iter().any(|attr| attr.path().is_ident("test") || attr.path().is_ident("bench")) {
            Some("it is a test".to_owned())
        } else {
            None
        };
        if let Some(reason) = reason {
            self.report.skipped("hook point", label, &reason);
            return;
        }
        let piece = Piece::new(&self.module, label, Part::HookPoint);
        if let Some(reason) = self.dropped(attrs, &piece) {
            self.report.skipped("hook point", label, &reason);
            return;
        }
        match hookpoints::hook_point(self.source, attrs, sig, label) {
            Ok(point) => {
                // An attribute macro may report trouble with the hook point at its own tokens or
                // the function's: those count against the hook point too.
                let macro_span = outer.or_else(|| {
                    functions::unknown_attribute(attrs).map(|_| {
                        let start = attrs
                            .first()
                            .map_or_else(|| self.source.start(sig.span()), |attr| self.source.start(attr.span()));
                        (start, self.source.end(block.brace_token.span.close()))
                    })
                });
                if let Some((start, end)) = macro_span {
                    self.edits.mark(start, end, piece.clone());
                }
                self.edits.insert_piece(self.source.end(block.brace_token.span.open()), point.prefix, piece.clone());
                self.edits.insert_piece(self.source.start(block.brace_token.span.close()), point.suffix, piece);
                self.hook_points += 1;
            }
            Err(reason) => self.report.skipped("hook point", label, &reason),
        }
    }

    /// Hook points for the methods of a module-level `impl` block.
    fn impl_hook_points(&mut self, item: &ItemImpl) {
        if self.cx.hook_points.is_none() || matches!(&item.trait_, Some((Some(_), _, _))) {
            return;
        }
        let self_name = match &*item.self_ty {
            Type::Path(path) => path.path.segments.last().map(|segment| segment.ident.to_string()),
            _ => None,
        }
        .unwrap_or_else(|| functions::compact(&item.self_ty));
        // An attribute macro on the block may report trouble with any method's hook point there.
        let block = functions::unknown_attribute(&item.attrs)
            .map(|_| (self.source.start(item.span()), self.source.end(item.brace_token.span.close())));
        for impl_item in &item.items {
            let syn::ImplItem::Fn(method) = impl_item else { continue };
            let name = method.sig.ident.to_string();
            let label = match &item.trait_ {
                Some((_, path, _)) => format!("<{self_name} as {}>::{name}", functions::compact(path)),
                None => format!("{self_name}::{name}"),
            };
            self.hook_point(&method.attrs, &method.sig, &method.block, &label, &[self_name.clone(), name], block);
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

/// One instantiation of a generic impl block: its type arguments, and the methods called with them.
struct Instance {
    args: Vec<String>,
    called: HashSet<String>,
}

/// The type arguments among MIR's generic arguments: lifetimes (`'_`) dropped.
fn type_args(args: &[String]) -> Vec<String> {
    args.iter().filter(|arg| !arg.starts_with('\'')).cloned().collect()
}

const TRAIT_METHODS: &str = " #[doc(hidden)] #[allow(dead_code)] \
    fn __scry_ref(&self) -> ::scry::__private::FieldRef<'_> { \
    ::scry::__private::FieldRef::opaque(::core::any::type_name::<Self>()) } \
    #[doc(hidden)] #[allow(dead_code)] \
    fn __scry_mut(&mut self) -> ::scry::__private::FieldMut<'_> { \
    ::scry::__private::FieldMut::Other(::core::any::type_name::<Self>()) } ";

const IMPL_METHODS: &str = " #[allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)] \
    fn __scry_ref(&self) -> ::scry::__private::FieldRef<'_> { \
    ::scry::__private::probe_ref!(self) } \
    #[allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)] \
    fn __scry_mut(&mut self) -> ::scry::__private::FieldMut<'_> { \
    ::scry::__private::probe_mut!(self) } ";

/// Registers a static, with a peeler for its type (see `scry::peel`) so a foreign type inside a
/// container the static's type names is reached.
/// `cfgs` are the static's own `#[cfg]` attributes: a static that only exists in some builds is
/// only registered in those.
fn static_registration(name: &str, path: &str, ty: &str, cfgs: &str) -> String {
    format!(
        " {cfgs} #[::scry::__private::linkme::distributed_slice(::scry::__private::STATICS)] \
         #[linkme(crate = ::scry::__private::linkme)] #[doc(hidden)] #[allow(non_upper_case_globals, clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)] \
         static __SCRY_STATIC_{name}: ::scry::__private::StaticEntry = {{ \
         fn __scry_register(registry: &mut ::scry::Registry) {{ \
         registry.__add_field(::core::concat!(::core::module_path!(), \"::\", {path:?}), \
         ::scry::__private::probe_ref!(&{name}, ::scry::__private::peeler_for!({ty}))); }} __scry_register }};"
    )
}

/// Registers a `const` as a read-only root. A const has no address, so the registration evaluates
/// it once into a leaked copy; one registry is built per endpoint, so this happens once.
fn const_registration(name: &str, path: &str, ty: &str, cfgs: &str) -> String {
    format!(
        " {cfgs} #[::scry::__private::linkme::distributed_slice(::scry::__private::STATICS)]          #[linkme(crate = ::scry::__private::linkme)] #[doc(hidden)] #[allow(non_upper_case_globals, clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]          static __SCRY_CONST_{name}: ::scry::__private::StaticEntry = {{          fn __scry_register(registry: &mut ::scry::Registry) {{          let value: &'static {ty} = ::std::boxed::Box::leak(::std::boxed::Box::new({name}));          registry.__add_const(::core::concat!(::core::module_path!(), \"::\", {path:?}),          ::scry::__private::probe_ref!(value, ::scry::__private::peeler_for!({ty}))); }} __scry_register }};"
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
            clause
                .predicates
                .iter()
                .any(|predicate| matches!(predicate, WherePredicate::Type(predicate) if maybe(&predicate.bounds)))
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
    /// The `#[cfg]` attributes on the declaration and on the inline modules it sits in.
    pub cfgs: Vec<syn::Attribute>,
}

pub struct ModCollector<'a> {
    source: &'a Source,
    inline_path: Vec<String>,
    inline_cfgs: Vec<syn::Attribute>,
    pub decls: Vec<ModDecl>,
}

impl<'a> ModCollector<'a> {
    pub fn new(source: &'a Source) -> Self {
        Self { source, inline_path: Vec::new(), inline_cfgs: Vec::new(), decls: Vec::new() }
    }
}

fn cfg_attrs(attrs: &[syn::Attribute]) -> impl Iterator<Item = syn::Attribute> + '_ {
    attrs.iter().filter(|attr| attr.path().is_ident("cfg")).cloned()
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
            let mut cfgs = self.inline_cfgs.clone();
            cfgs.extend(cfg_attrs(&item.attrs));
            self.decls.push(ModDecl {
                name,
                inline_path: self.inline_path.clone(),
                path_attr,
                path_literal,
                insert_at,
                cfgs,
            });
        } else {
            let outer = self.inline_cfgs.len();
            self.inline_cfgs.extend(cfg_attrs(&item.attrs));
            self.inline_path.push(name);
            visit::visit_item_mod(self, item);
            self.inline_path.pop();
            self.inline_cfgs.truncate(outer);
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
    if item
        .generics
        .params
        .iter()
        .any(|param| matches!(param, GenericParam::Type(param) if param.default.as_ref().is_some_and(mentions_self)))
    {
        return Err("a type parameter defaults to Self".into());
    }
    if item
        .generics
        .where_clause
        .as_ref()
        .is_some_and(|clause| clause.predicates.iter().any(|p| mentions_self(&p.to_token_stream())))
    {
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

/// The components the crate launches - `dioxus::launch(App)`, `LaunchBuilder::new().launch(App)`,
/// `builder.launch(layout::Root)` - anywhere in it, not only in `main`: an app may launch from a
/// `run()` that `main` dispatches to. They pump the endpoint's requests on the UI thread. Matched
/// by the component's name, and only a capitalized one, as components are, so a `launch(config)`
/// of something else is not mistaken for one. Of several, the first to mount pumps. Code a `#[cfg]`
/// turns off in this build is left out ([`Cfg`]): a server built from the same crate as its Dioxus
/// client launches nothing.
pub struct LaunchCollector<'a> {
    pub roots: HashSet<String>,
    cfg: &'a Cfg,
}

impl<'a> LaunchCollector<'a> {
    pub fn new(cfg: &'a Cfg) -> Self {
        Self { roots: HashSet::new(), cfg }
    }

    fn root(&mut self, args: &syn::punctuated::Punctuated<Expr, syn::Token![,]>) {
        if let Some(Expr::Path(path)) = args.first()
            && path.qself.is_none()
            && let Some(last) = path.path.segments.last()
            && last.ident.to_string().starts_with(|c: char| c.is_ascii_uppercase())
        {
            self.roots.insert(last.ident.to_string());
        }
    }
}

impl<'ast> Visit<'ast> for LaunchCollector<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs = match item {
            syn::Item::Fn(item) => &item.attrs,
            syn::Item::Mod(item) => &item.attrs,
            syn::Item::Impl(item) => &item.attrs,
            syn::Item::Trait(item) => &item.attrs,
            syn::Item::Const(item) => &item.attrs,
            syn::Item::Static(item) => &item.attrs,
            syn::Item::Macro(item) => &item.attrs,
            _ => return visit::visit_item(self, item),
        };
        if !self.cfg.off(attrs) {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if !self.cfg.off(&item.attrs) {
            visit::visit_impl_item_fn(self, item);
        }
    }

    fn visit_stmt(&mut self, stmt: &'ast syn::Stmt) {
        let attrs = match stmt {
            syn::Stmt::Local(local) => &local.attrs,
            syn::Stmt::Expr(Expr::Call(call), _) => &call.attrs,
            syn::Stmt::Expr(Expr::MethodCall(call), _) => &call.attrs,
            syn::Stmt::Expr(Expr::Block(block), _) => &block.attrs,
            _ => return visit::visit_stmt(self, stmt),
        };
        if !self.cfg.off(attrs) {
            visit::visit_stmt(self, stmt);
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(func) = &*call.func
            && func.path.segments.last().is_some_and(|segment| segment.ident == "launch")
        {
            self.root(&call.args);
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "launch" {
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
        rewrite_with_mir(text, hooks, "")
    }

    /// The same, with `mir` as the crate's MIR dump.
    fn rewrite_with_mir(text: &str, hooks: bool, mir: &str) -> (String, String) {
        rewrite_with(text, hooks, mir, None)
    }

    fn rewrite_with(text: &str, hooks: bool, mir: &str, hook_points: Option<&Config>) -> (String, String) {
        rewrite_full(text, hooks, mir, hook_points, &Dropped::default())
    }

    /// The same, leaving out the pieces in `dropped`.
    fn rewrite_dropping(text: &str, hook_points: Option<&Config>, dropped: &Dropped) -> (String, String) {
        rewrite_full(text, false, "", hook_points, dropped)
    }

    fn rewrite_full(
        text: &str,
        hooks: bool,
        mir: &str,
        hook_points: Option<&Config>,
        dropped: &Dropped,
    ) -> (String, String) {
        let source = Source::new(text.to_owned());
        let syntax = syn::parse_file(text).unwrap();
        let mut report = Report::default();
        let mut traits = TraitCollector::default();
        traits.visit_file(&syntax);
        let dyn_traits = resolve_dyn_traits(&traits.traits, &mut report);
        let mut index = Index::default();
        index.add_file(&[], &syntax);
        let uses = generics::scan(mir, &index.generic_names);
        let cx = Context {
            dyn_traits: &dyn_traits,
            hooks,
            endpoint: None,
            index: &index,
            uses: &uses,
            generics: true,
            crate_name: "app",
            hook_points,
            dropped,
        };
        let mut edits = Edits::default();
        Generator::new(&source, &mut edits, &cx, &mut report, true, Vec::new()).visit_file(&syntax);
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
        assert!(out.contains("impl Rule for u8 { #[allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)] fn __scry_ref"));
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
    fn registers_consts_as_read_only_roots() {
        let (out, report) = rewrite(
            "const A: &[u8] = &[1];
const _: () = ();
fn f() { const B: u8 = 3; }
",
            false,
        );
        assert!(out.contains("static __SCRY_CONST_A"), "{out}");
        assert!(
            out.contains("let value: &'static & [u8] = ::std::boxed::Box::leak(::std::boxed::Box::new(A))"),
            "{out}"
        );
        assert!(out.contains("registry.__add_const"));
        assert!(out.contains("\"f::B\""));
        assert!(!out.contains("__SCRY_CONST__"), "an unnamed const has nothing to read");
        assert!(report.contains("const A") && report.contains("const f::B"), "{report}");
    }

    #[test]
    fn tracks_state_hooks_only_with_dioxus() {
        let text = "fn App() { let mut count = use_signal(|| 0);\nlet other = make(); }\n";
        let (out, report) = rewrite(text, true);
        assert!(
            out.contains("{ let __scry_hook = use_signal(|| 0); ::scry::__private::dioxus::track(\"App\", \"count\"")
        );
        assert!(report.contains("hook App.count (use_signal)"));
        assert_eq!(report.matches("hook").count(), 1);
        assert_eq!(rewrite(text, false).0, text);
    }

    #[test]
    fn tracks_resources_actions_and_root_contexts() {
        let text = "fn Page() { let stats = use_resource(move || load());\n\
                    let mut save = use_action(move || store());\n\
                    let theme = use_root_context(|| Theme::Dark);\n\
                    let later = use_server_future(load)?; }\n";
        let (_, report) = rewrite(text, true);
        for hook in ["Page.stats (use_resource)", "Page.save (use_action)", "Page.theme (use_root_context)"] {
            assert!(report.contains(&format!("hook {hook}")), "{report}");
        }
        assert!(!report.contains("later"), "a hook behind `?` holds no handle until it resolves");
    }

    #[test]
    fn emits_call_shims_for_callable_functions_only() {
        let text = "fn add(a: i64, b: &str) -> i64 { a }
#[cfg(windows)] fn only_windows() {}
fn generic<T>(t: T) {}
async fn later() {}
fn soon() -> impl std::future::Future<Output = u8> { async { 1 } }
fn takes_mut(v: &mut Vec<u8>, text: &mut str) {}
fn takes_dyn(v: &mut dyn std::io::Write) {}
fn takes_impl(v: impl Fn()) {}
fn takes_into(name: impl Into<String>, path: impl AsRef<std::path::Path>, items: impl IntoIterator<Item = u32>, it: impl Iterator<Item = u8> + Send) {}
fn takes_copy(v: impl Into<String> + Copy) {}
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
        for callable in [
            "fn add",
            "fn only_windows",
            "fn takes_mut",
            "fn takes_into",
            "fn later",
            "fn soon",
            "method S::get",
            "method S::bump",
            "method S::new",
            "method S::eat",
            "method S::same",
            "method S::default",
        ] {
            assert!(
                report.contains(&format!("+ {callable}")),
                "{callable} missing:
{report}"
            );
        }
        for (skipped, reason) in [
            ("fn generic", "never calls it with concrete types"),
            ("fn takes_dyn", "`&mut dyn Trait`"),
            ("fn takes_impl", "it takes `impl Fn ()`, which no type is picked for"),
            ("fn takes_copy", "which no type is picked for"),
            ("fn card", "UI component"),
            ("fn App", "UI component"),
            ("impl Wrap<T>", "never calls its methods with concrete types"),
        ] {
            assert!(
                report.lines().any(|line| line.contains(skipped) && line.contains(reason)),
                "{skipped}:
{report}"
            );
        }
        // Attempted under a macro not known to keep it; dropped later only if it doesn't compile.
        assert!(report.contains("+ fn wrapped (under #[tokio::main])"), "{report}");
        assert!(!report.contains("fn main") && !report.contains("inner"), "{report}");

        let shim = |name: &str| out.lines().find(|line| line.contains(&format!("fn {name}"))).unwrap().to_owned();
        assert!(shim("only_windows").contains("} # [cfg (windows)] # [:: scry"), "{}", shim("only_windows"));
        let add = shim("add");
        assert!(add.contains("probe_arg ! (i64") && add.contains("probe_ref_arg ! (str"), "{add}");
        assert!(add.contains("signature : \"fn add(a: i64, b: &str) -> i64\""), "{add}");
        assert!(shim("later").contains("block_on (later ())"), "{}", shim("later"));
        assert!(shim("soon").contains("block_on (soon ())"), "{}", shim("soon"));
        let takes_into = shim("takes_into");
        for built in [
            "probe_arg ! (String",
            "probe_arg ! (:: std :: path :: PathBuf",
            "probe_arg ! (:: std :: vec :: Vec < u32 >",
        ] {
            assert!(takes_into.contains(built), "{built}: {takes_into}");
        }
        assert!(takes_into.contains(":: core :: iter :: IntoIterator :: into_iter (arg3)"), "{takes_into}");
        let takes_mut = shim("takes_mut");
        assert!(
            takes_mut.contains("let mut arg0 : Vec < u8 >")
                && takes_mut.contains("(& mut arg0 , arg1 . as_mut_str ())"),
            "{takes_mut}"
        );
        assert!(
            takes_mut.contains("\"args\"") && takes_mut.contains("String :: from (\"text\")"),
            "the changed values are reported: {takes_mut}"
        );
        let methods = shim("get");
        assert!(shim("default").contains("< S as Default > :: default"), "{}", shim("default"));
        assert!(methods.contains("probe_ref_arg ! (S ,"), "Self is spelled as the impl type: {methods}");
        assert!(methods.contains("probe_ref_arg ! (str"), "a named lifetime is dropped: {methods}");
        assert!(methods.contains("SelfProbe (this)"), "{methods}");
        assert!(methods.contains("Receiver :: Mut"), "{methods}");
    }

    #[test]
    fn instantiates_generics_the_crate_calls() {
        let text = "pub struct Item;
mod hidden { struct Secret; }
pub fn show<T: Clone>(value: &T, times: u32) -> u32 { times }
pub fn never<T>(t: T) {}
pub struct Cache<K, V> { k: K, v: V }
impl<K: Clone, V> Cache<K, V> { pub fn get(&self, key: &K) -> Option<&V> { None } pub fn sum(&self) -> u8 where V: Copy { 0 } }
impl<T> Cache<T, T> { fn same(&self) {} }
";
        let mir = "_1 = show::<Item>(copy _2, const 1_u32) -> [return: bb1];
_3 = show::<Vec<String>>(copy _4, const 2_u32) -> [return: bb2];
_5 = show::<hidden::Secret>(copy _6, const 3_u32) -> [return: bb3];
_6 = show::<T>(copy _6, const 3_u32) -> [return: bb3];
_7 = Cache::<u32, Item>::get(copy _8, copy _9) -> [return: bb4];
let _9: Cache<u8, u8>;
";
        let (out, report) = rewrite_with_mir(text, false, mir);
        for generated in ["+ fn show::<Item>", "+ fn show::<Vec<String>>", "+ method Cache<u32, Item>::get"] {
            assert!(
                report.contains(generated),
                "{generated} missing:
{report}"
            );
        }
        for (skipped, reason) in [
            ("fn show", "`hidden::Secret` cannot be named"),
            ("fn never", "never calls it with concrete types"),
            ("method Cache<u32, Item>::sum", "bounds of its own"),
            ("impl Cache<T, T>", "specific shape"),
        ] {
            assert!(
                report.lines().any(|line| line.contains(skipped) && line.contains(reason)),
                "{skipped}:
{report}"
            );
        }
        let show = out.lines().find(|line| line.starts_with("pub fn show")).unwrap();
        assert!(show.contains("show :: < crate :: Item >"), "{show}");
        assert!(show.contains("probe_ref_arg ! (crate :: Item"), "T is replaced in parameters: {show}");
        assert!(show.contains("show :: < Vec < String > >"), "{show}");
        let cache = out.lines().find(|line| line.starts_with("impl<K: Clone, V>")).unwrap();
        assert!(cache.contains("TypeId :: of :: < Cache < u32 , crate :: Item > >"), "{cache}");
        assert!(cache.contains("probe_ref_arg ! (u32"), "K is replaced: {cache}");
    }

    #[test]
    fn handles_patterns_from_a_real_app() {
        let text = "#[cfg(feature = \"server\")] static GLOBAL: u8 = 0;
fn keep(name: &'static str) {}
fn verify(cert: &Cert<'_>, names: &[&'static str]) {}
";
        let (out, report) = rewrite(text, false);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].contains("# [cfg (feature = \"server\")] #[::scry::__private::linkme"),
            "a static's cfg is kept: {}",
            lines[0]
        );
        assert!(lines[1].contains("Box :: leak"), "a 'static parameter gets a value that lives for good: {}", lines[1]);
        assert!(
            lines[2].contains("Unconstructible < Cert < '_ > >") && lines[2].contains("arg0 . lend ()"),
            "{}",
            lines[2]
        );
        assert!(
            lines[2].contains("Unconstructible < [& 'static str] >") && lines[2].contains("arg1 . lend ()"),
            "{}\n{report}",
            lines[2]
        );
    }

    #[test]
    fn finds_launched_root_components_anywhere_in_the_crate() {
        let file = syn::parse_file(
            "fn main() { client::run() }
mod client { pub fn run() { let app = boot(); app.launch(layout::RootLayout); } }
fn other() { dioxus::launch(App); rocket.launch(config); Server::launch(settings); }
",
        )
        .unwrap();
        let cfg = Cfg::default();
        let mut launches = LaunchCollector::new(&cfg);
        launches.visit_file(&file);
        let mut roots: Vec<&String> = launches.roots.iter().collect();
        roots.sort();
        assert_eq!(roots, ["App", "RootLayout"], "only capitalized components, by their last segment");
    }

    #[test]
    fn leaves_out_launches_this_build_does_not_compile() {
        // One crate, built as a server or as its Dioxus client, as a fullstack app is.
        let file = syn::parse_file(
            r#"fn main() {
    #[cfg(feature = "server")]
    server::run();
    #[cfg(not(feature = "server"))]
    dioxus::launch(Splash);
}
#[cfg(not(feature = "server"))]
mod client { pub fn run() { app.launch(layout::RootLayout); } }
impl Shell { #[cfg(feature = "desktop")] fn open() { LaunchBuilder::new().launch(Desktop); } }
"#,
        )
        .unwrap();
        let server = Cfg::from_cfg_flags(["feature=\"server\""].into_iter());
        let mut launches = LaunchCollector::new(&server);
        launches.visit_file(&file);
        assert!(launches.roots.is_empty(), "{:?}", launches.roots);

        let client = Cfg::from_cfg_flags(["feature=\"desktop\""].into_iter());
        let mut launches = LaunchCollector::new(&client);
        launches.visit_file(&file);
        let mut roots: Vec<&String> = launches.roots.iter().collect();
        roots.sort();
        assert_eq!(roots, ["Desktop", "RootLayout", "Splash"]);
    }

    #[test]
    fn functions_under_any_attribute_macro_get_shims_and_hook_points() {
        let text = "#[tracing::instrument] fn traced(a: u32) -> u32 { a }
#[cached::proc_macro::cached] fn cached(a: u32) -> u32 { a }
#[other::rewrite] fn unknown(a: u32) -> u32 { a }
";
        let config = Config::default();
        let (out, report) = rewrite_with(text, false, "", Some(&config));
        for line in out.lines() {
            assert!(line.contains("static __SCRY_HOOK :") && line.contains("__SCRY_FN_"), "{line}\n{report}");
        }
        // Only the one not known to keep its function is attempted, as the report says.
        assert!(report.contains("+ fn unknown (under #[other::rewrite])"), "{report}");
        assert!(report.contains("+ fn traced\n"), "{report}");
    }

    #[test]
    fn pieces_that_did_not_compile_are_left_out() {
        let text = "#[web::get] fn index() -> u32 { 1 }
#[web::post] fn submit(a: u32) -> u32 { a }
struct Api;
#[web::routes] impl Api { fn list(&self) -> u32 { 2 } fn count(&self) -> u32 { 3 } }
";
        let mut dropped = Dropped::default();
        dropped.pieces.insert(Piece::new(&[], "index", Part::Shim));
        dropped.pieces.insert(Piece::new(&[], "submit", Part::HookPoint));
        dropped.pieces.insert(Piece::new(&[], "Api::list", Part::Shim));
        let config = Config::default();
        let (out, report) = rewrite_dropping(text, Some(&config), &dropped);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("__SCRY_HOOK") && !lines[0].contains("__SCRY_FN_"), "{}", lines[0]);
        assert!(!lines[1].contains("__SCRY_HOOK") && lines[1].contains("__SCRY_FN_"), "{}", lines[1]);
        assert_eq!(lines[3].matches("__SCRY_FN_").count(), 1, "{}", lines[3]);
        assert!(
            report.contains("- fn index: skipped, its call shim did not compile under its #[web::get] attribute"),
            "{report}"
        );
        assert!(report.contains("- hook point submit: skipped, its hook point did not compile"), "{report}");
        assert!(report.contains("- method Api::list: skipped, its call shim did not compile under its #[web::routes]"));
        assert!(report.contains("+ method Api::count (under #[web::routes])"), "{report}");
    }

    #[test]
    fn compiles_hook_points_into_hookable_functions() {
        let text = "pub fn ratio(a: u32, b: u32) -> f64 {
    a as f64 / b as f64
}
pub async fn fetch(id: u32) -> Result<u32, String> {
    Ok(id)
}
struct S;
impl S { fn get(&mut self, (a, b): (u8, u8), seen: &u8, out: &mut u8) -> &mut S { self } }
impl Clone for S { fn clone(&self) -> Self { S } }
const fn fixed() -> u8 { 1 }
fn never() -> ! { loop {} }
fn main() {}
";
        let config = Config::default();
        let (out, report) = rewrite_with(text, false, "", Some(&config));
        assert_eq!(out.matches("static __SCRY_HOOK :").count(), 4, "{report}");
        for skipped in ["hook point fixed: skipped, it is a const fn", "hook point never: skipped, it returns `!`"] {
            assert!(
                report.contains(skipped),
                "{skipped}:
{report}"
            );
        }
        assert!(!report.contains("hook point main"), "main gets no hook point: {report}");

        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains(r#""::ratio""#) && lines[0].contains("run_once(move || -> f64 {"), "{}", lines[0]);
        assert!(lines[0].contains(r#"__scry_capture . arg ("a""#), "{}", lines[0]);
        assert_eq!(lines[1], "    a as f64 / b as f64", "the body's own lines are untouched");
        assert!(lines[2].starts_with(" }), }; # [allow"), "{}", lines[2]);
        assert!(lines[0].contains("let mut a = a ;"), "a by-value parameter is rebound to be changeable");
        assert!(lines[0].contains(r#"__scry_changes . arg ("a")"#), "{}", lines[0]);
        assert!(lines[0].contains("let mut __scry_result: f64 = match __scry_skip {"), "{}", lines[0]);
        assert!(lines[2].contains("__scry_c . after"), "{}", lines[2]);
        assert!(
            lines[3].contains("let mut __scry_result: Result < u32 , String > = match __scry_skip {"),
            "{}",
            lines[3]
        );
        assert!(lines[3].contains("=> async move {"), "{}", lines[3]);
        assert!(lines[5].contains("}.await, };"), "{}", lines[5]);
        let method = lines[7];
        assert!(method.contains(r#""::S::get""#) && method.contains("__scry_capture . this"), "{method}");
        assert!(!method.contains(r#"arg ("a""#), "a destructured parameter is not captured: {method}");
        assert!(method.contains(r#"__scry_changes . arg ("out")"#), "a &mut parameter can be changed: {method}");
        assert!(!method.contains(r#"__scry_changes . arg ("seen")"#), "a &T parameter cannot: {method}");
        assert!(method.contains("probe_mut ! (& mut * self)"), "a &mut self receiver can be changed: {method}");
        assert!(method.contains("let mut out = out ;"), "a &mut parameter is rebound too: {method}");
        assert!(lines[8].contains(r#""::<S as Clone>::clone""#), "{}", lines[8]);
    }
}
