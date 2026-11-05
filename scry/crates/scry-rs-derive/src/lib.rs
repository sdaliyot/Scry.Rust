//! `#[derive(Reflect)]` for Scry.Rust.
//!
//! The generated impl sits next to the type, in the same module, which is what lets it read and
//! write private fields. Each field goes through the autoref probes in `scry::__private`, so a
//! field whose type does not implement `Reflect` degrades to its `Debug` output or its type name
//! instead of breaking the build.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::{Data, DataEnum, DeriveInput, Fields, Ident, Index, Member, Type, Variant, parse_macro_input, parse_quote};

#[proc_macro_derive(Reflect)]
pub fn derive_reflect(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input).unwrap_or_else(syn::Error::into_compile_error).into()
}

fn expand(mut input: DeriveInput) -> syn::Result<Tokens> {
    // A type with generic parameters may not be `'static`, so it gets neither `as_any` - and with
    // it, callable methods - nor `FromValue`.
    let concrete = input.generics.params.is_empty();

    // A field built from a type parameter is only navigable if the parameter is.
    for parameter in input.generics.type_params_mut() {
        parameter.bounds.push(parse_quote!(::scry::Reflect));
    }

    let body = match &input.data {
        Data::Struct(data) => expand_struct(&data.fields),
        Data::Enum(data) => expand_enum(data),
        Data::Union(data) => {
            return Err(syn::Error::new_spanned(data.union_token, "Reflect cannot be derived for unions"));
        }
    };

    let name = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();
    let (as_any, from_value) = if concrete {
        let construct = match &input.data {
            Data::Struct(data) if !unsized_tail(&data.fields) => Some(construct_struct(&data.fields)),
            Data::Enum(data) => Some(construct_enum(data)),
            _ => None,
        };
        let from_value = construct.map(|construct| {
            quote! {
                #[automatically_derived]
                #[allow(unused_variables, unused_imports, unreachable_patterns, clippy::all)]
                impl ::scry::__private::FromValue for #name {
                    fn from_value(value: &::scry::Value) -> ::core::result::Result<Self, ::scry::Error> {
                        use ::scry::__private::{ViaFromValue as _, ViaNoValue as _};
                        #construct
                    }
                }
            }
        });
        let as_any = quote! {
            fn as_any(&self) -> ::core::option::Option<&dyn ::core::any::Any> {
                ::core::option::Option::Some(self)
            }

            fn as_any_mut(&mut self) -> ::core::option::Option<&mut dyn ::core::any::Any> {
                ::core::option::Option::Some(self)
            }
        };
        (as_any, from_value)
    } else {
        (Tokens::new(), None)
    };
    Ok(quote! {
        #[automatically_derived]
        #[allow(unused_variables, unused_imports, unreachable_patterns, clippy::all)]
        impl #impl_generics ::scry::Reflect for #name #type_generics #where_clause {
            fn type_name(&self) -> &'static str {
                ::core::any::type_name::<Self>()
            }

            #as_any

            #body
        }

        #from_value
    })
}

/// A last field such as `[u8]` or `str` makes the type unsized, and unable to be built.
fn unsized_tail(fields: &Fields) -> bool {
    fields.iter().next_back().is_some_and(|field| match &field.ty {
        Type::Slice(_) | Type::TraitObject(_) => true,
        Type::Path(path) => path.path.is_ident("str"),
        _ => false,
    })
}

fn arg_probe(ty: &Type, value: Tokens) -> Tokens {
    quote!((&&&::scry::__private::ArgProbe::<#ty>::new()).scry_arg(#value)?)
}

/// Builds `constructor` (`Self`, `Self::Variant`) with `fields` from the JSON `value`: an object
/// of named fields; for unnamed fields the object `snapshot` produces (`{"0": .., "1": ..}`),
/// or the value itself for one field and an array for several, as serde writes them.
fn construct_fields(constructor: Tokens, fields: &Fields, value: Tokens) -> Tokens {
    let list = fields_of(fields);
    match fields {
        Fields::Named(_) => {
            let names: Vec<&String> = list.iter().map(|field| &field.name).collect();
            let members = list.iter().map(|field| &field.member);
            let values = list.iter().map(|field| {
                let name = &field.name;
                arg_probe(field.ty, quote!(object.get(#name).unwrap_or(&::scry::Value::Null)))
            });
            quote! {{
                let object = #value
                    .as_object()
                    .ok_or_else(|| ::scry::__private::mismatch("an object of fields", #value))?;
                if let ::core::option::Option::Some(key) = object.keys().find(|key| ![#(#names),*].contains(&key.as_str())) {
                    return ::core::result::Result::Err(::scry::Error::NoSuchMember {
                        type_name: ::core::any::type_name::<Self>(),
                        member: ::std::clone::Clone::clone(key),
                    });
                }
                ::core::result::Result::Ok(#constructor { #(#members: #values),* })
            }}
        }
        Fields::Unnamed(_) => {
            let count = list.len();
            let expected = format!("an array of {count}, or an object of fields \"0\" to \"{}\"", count - 1);
            let names: Vec<&String> = list.iter().map(|field| &field.name).collect();
            let values = list.iter().enumerate().map(|(index, field)| arg_probe(field.ty, quote!(&items[#index])));
            quote! {{
                let items: ::std::vec::Vec<&::scry::Value> = match (#value.as_object(), #value.as_array()) {
                    (::core::option::Option::Some(object), _) if object.len() == #count && [#(#names),*].iter().all(|name| object.contains_key(*name)) => {
                        [#(#names),*].iter().map(|name| &object[*name]).collect()
                    }
                    (_, ::core::option::Option::Some(items)) if #count > 1 && items.len() == #count => items.iter().collect(),
                    _ if #count == 1 => ::std::vec![#value],
                    _ => return ::core::result::Result::Err(::scry::__private::mismatch(#expected, #value)),
                };
                ::core::result::Result::Ok(#constructor(#(#values),*))
            }}
        }
        Fields::Unit => quote!(::core::result::Result::Ok(#constructor)),
    }
}

fn construct_struct(fields: &Fields) -> Tokens {
    construct_fields(quote!(Self), fields, quote!(value))
}

/// A unit variant from its name as a string; any variant from `{"Variant": fields}`.
fn construct_enum(data: &DataEnum) -> Tokens {
    let mut unit_names = Vec::new();
    let mut unit_idents = Vec::new();
    let mut names = Vec::new();
    let mut builds = Vec::new();
    for variant in &data.variants {
        let ident = &variant.ident;
        let name = ident.unraw().to_string();
        if matches!(variant.fields, Fields::Unit) {
            unit_names.push(name.clone());
            unit_idents.push(ident.clone());
        }
        builds.push(construct_fields(quote!(Self::#ident), &variant.fields, quote!(fields)));
        names.push(name);
    }
    let expected = format!("a variant: {}", names.join(", "));
    quote! {
        if let ::core::option::Option::Some(name) = value.as_str() {
            return match name {
                #(#unit_names => ::core::result::Result::Ok(Self::#unit_idents),)*
                _ => ::core::result::Result::Err(::scry::__private::mismatch(#expected, value)),
            };
        }
        let ::core::option::Option::Some((name, fields)) = value.as_object().filter(|object| object.len() == 1).and_then(|object| object.iter().next()) else {
            return ::core::result::Result::Err(::scry::__private::mismatch(#expected, value));
        };
        match name.as_str() {
            #(#names => #builds,)*
            _ => ::core::result::Result::Err(::scry::__private::mismatch(#expected, value)),
        }
    }
}

struct Field<'a> {
    member: Member,
    name: String,
    ty: &'a Type,
    binding: Ident,
}

fn fields_of(fields: &Fields) -> Vec<Field<'_>> {
    fields
        .iter()
        .enumerate()
        .map(|(position, field)| {
            let (member, name) = match &field.ident {
                Some(ident) => (Member::Named(ident.clone()), ident.unraw().to_string()),
                None => (Member::Unnamed(Index::from(position)), position.to_string()),
            };
            Field { member, name, ty: &field.ty, binding: format_ident!("__scry_field_{}", position) }
        })
        .collect()
}

/// Imports for the probes, so `scry_ref`/`scry_mut` resolve through autoref specialization.
fn probe_imports() -> Tokens {
    quote! {
        use ::scry::__private::{ViaDebug as _, ViaOpaque as _, ViaOtherMut as _, ViaReflect as _, ViaReflectMut as _};
    }
}

fn read_probe(access: Tokens) -> Tokens {
    quote!((&&&::scry::__private::Probe(#access)).scry_ref())
}

fn write_probe(access: Tokens) -> Tokens {
    quote!((&&&::scry::__private::ProbeMut::new(#access)).scry_mut())
}

fn expand_struct(fields: &Fields) -> Tokens {
    let fields = fields_of(fields);
    let kind = if fields.first().is_some_and(|field| matches!(field.member, Member::Unnamed(_))) {
        quote!(::scry::Kind::Tuple)
    } else {
        quote!(::scry::Kind::Struct)
    };
    let imports = probe_imports();
    let names: Vec<&String> = fields.iter().map(|field| &field.name).collect();
    let types: Vec<&Type> = fields.iter().map(|field| field.ty).collect();
    let reads: Vec<Tokens> = fields.iter().map(|field| {
        let member = &field.member;
        read_probe(quote!(&self.#member))
    }).collect();
    let writes: Vec<Tokens> = fields.iter().map(|field| {
        let member = &field.member;
        write_probe(quote!(&mut self.#member))
    }).collect();

    quote! {
        fn kind(&self) -> ::scry::Kind {
            #kind
        }

        fn members(&self) -> ::std::vec::Vec<::scry::Member> {
            ::std::vec![#(::scry::Member::new(#names, ::core::any::type_name::<#types>())),*]
        }

        fn snapshot(&self, depth: usize) -> ::scry::Value {
            #imports
            if depth == 0 {
                return ::scry::__private::truncated(self.type_name());
            }
            let mut object = ::scry::__private::Map::new();
            #(object.insert(::std::string::String::from(#names), #reads.snapshot(depth - 1));)*
            ::scry::Value::Object(object)
        }

        fn read(&self, path: &[::scry::Segment], visit: ::scry::Visit<'_>) -> ::core::result::Result<(), ::scry::Error> {
            #imports
            let ::core::option::Option::Some((segment, rest)) = path.split_first() else {
                return visit(self);
            };
            match &*segment.name() {
                #(#names => #reads.read(rest, visit),)*
                _ => ::core::result::Result::Err(::scry::__private::no_member(self.type_name(), segment)),
            }
        }

        fn write(&mut self, path: &[::scry::Segment], visit: ::scry::VisitMut<'_>) -> ::core::result::Result<(), ::scry::Error> {
            #imports
            let ::core::option::Option::Some((segment, rest)) = path.split_first() else {
                return visit(self);
            };
            let type_name = self.type_name();
            match &*segment.name() {
                #(#names => #writes.write(rest, visit),)*
                _ => ::core::result::Result::Err(::scry::__private::no_member(type_name, segment)),
            }
        }

        fn write_shared(&self, path: &[::scry::Segment], visit: ::scry::VisitMut<'_>) -> ::core::result::Result<(), ::scry::Error> {
            #imports
            let ::core::option::Option::Some((segment, rest)) = path.split_first() else {
                return ::core::result::Result::Err(::scry::__private::needs_interior_mutability(self.type_name()));
            };
            match &*segment.name() {
                #(#names => #reads.write_shared(rest, visit),)*
                _ => ::core::result::Result::Err(::scry::__private::no_member(self.type_name(), segment)),
            }
        }

        /// Assigns the fields present in a JSON object; fields it does not mention are untouched.
        fn assign(&mut self, value: &::scry::Value) -> ::core::result::Result<(), ::scry::Error> {
            #imports
            let type_name = self.type_name();
            let object = value
                .as_object()
                .ok_or_else(|| ::scry::__private::mismatch("an object of fields", value))?;
            for (key, field_value) in object {
                match key.as_str() {
                    #(#names => #writes.assign(field_value)?,)*
                    _ => {
                        return ::core::result::Result::Err(::scry::Error::NoSuchMember {
                            type_name,
                            member: ::std::clone::Clone::clone(key),
                        });
                    }
                }
            }
            ::core::result::Result::Ok(())
        }
    }
}

fn variant_pattern(variant: &Variant, fields: &[Field<'_>]) -> Tokens {
    let ident = &variant.ident;
    let bindings = fields.iter().map(|field| &field.binding);
    match &variant.fields {
        Fields::Named(_) => {
            let members = fields.iter().map(|field| &field.member);
            quote!(Self::#ident { #(#members: #bindings),* })
        }
        Fields::Unnamed(_) => quote!(Self::#ident(#(#bindings),*)),
        Fields::Unit => quote!(Self::#ident),
    }
}

fn expand_enum(data: &DataEnum) -> Tokens {
    let imports = probe_imports();
    let mut member_arms = Vec::new();
    let mut snapshot_arms = Vec::new();
    let mut read_arms = Vec::new();
    let mut write_arms = Vec::new();
    let mut write_shared_arms = Vec::new();
    let mut unit_variants = Vec::new();

    for variant in &data.variants {
        let fields = fields_of(&variant.fields);
        let pattern = variant_pattern(variant, &fields);
        let variant_name = variant.ident.unraw().to_string();
        let names: Vec<&String> = fields.iter().map(|field| &field.name).collect();
        let types: Vec<&Type> = fields.iter().map(|field| field.ty).collect();
        let reads: Vec<Tokens> = fields.iter().map(|field| {
            let binding = &field.binding;
            read_probe(quote!(#binding))
        }).collect();
        let writes: Vec<Tokens> = fields.iter().map(|field| {
            let binding = &field.binding;
            write_probe(quote!(#binding))
        }).collect();

        if fields.is_empty() {
            unit_variants.push((variant.ident.clone(), variant_name.clone()));
        }

        member_arms.push(quote! {
            #pattern => ::std::vec![#(::scry::Member::new(#names, ::core::any::type_name::<#types>())),*],
        });
        snapshot_arms.push(if fields.is_empty() {
            quote!(#pattern => ::scry::Value::String(::std::string::String::from(#variant_name)),)
        } else {
            quote! {
                #pattern => {
                    if depth == 0 {
                        return ::scry::__private::truncated(self.type_name());
                    }
                    let mut fields = ::scry::__private::Map::new();
                    #(fields.insert(::std::string::String::from(#names), #reads.snapshot(depth - 1));)*
                    let mut object = ::scry::__private::Map::new();
                    object.insert(::std::string::String::from(#variant_name), ::scry::Value::Object(fields));
                    ::scry::Value::Object(object)
                }
            }
        });
        read_arms.push(quote! {
            #pattern => match &*segment.name() {
                #(#names => #reads.read(rest, visit),)*
                _ => ::core::result::Result::Err(::scry::__private::no_member(type_name, segment)),
            },
        });
        write_arms.push(quote! {
            #pattern => match &*segment.name() {
                #(#names => #writes.write(rest, visit),)*
                _ => ::core::result::Result::Err(::scry::__private::no_member(type_name, segment)),
            },
        });
        write_shared_arms.push(quote! {
            #pattern => match &*segment.name() {
                #(#names => #reads.write_shared(rest, visit),)*
                _ => ::core::result::Result::Err(::scry::__private::no_member(type_name, segment)),
            },
        });
    }

    let unit_idents = unit_variants.iter().map(|(ident, _)| ident);
    let unit_names: Vec<&String> = unit_variants.iter().map(|(_, name)| name).collect();
    let expected = if unit_names.is_empty() {
        "a unit variant name (this enum has none)".to_owned()
    } else {
        format!("one of: {}", unit_names.iter().map(|name| name.as_str()).collect::<Vec<_>>().join(", "))
    };

    quote! {
        fn kind(&self) -> ::scry::Kind {
            ::scry::Kind::Enum
        }

        fn members(&self) -> ::std::vec::Vec<::scry::Member> {
            match self {
                #(#member_arms)*
            }
        }

        fn snapshot(&self, depth: usize) -> ::scry::Value {
            #imports
            match self {
                #(#snapshot_arms)*
            }
        }

        fn read(&self, path: &[::scry::Segment], visit: ::scry::Visit<'_>) -> ::core::result::Result<(), ::scry::Error> {
            #imports
            let ::core::option::Option::Some((segment, rest)) = path.split_first() else {
                return visit(self);
            };
            let type_name = self.type_name();
            match self {
                #(#read_arms)*
            }
        }

        fn write(&mut self, path: &[::scry::Segment], visit: ::scry::VisitMut<'_>) -> ::core::result::Result<(), ::scry::Error> {
            #imports
            let ::core::option::Option::Some((segment, rest)) = path.split_first() else {
                return visit(self);
            };
            let type_name = self.type_name();
            match self {
                #(#write_arms)*
            }
        }

        fn write_shared(&self, path: &[::scry::Segment], visit: ::scry::VisitMut<'_>) -> ::core::result::Result<(), ::scry::Error> {
            #imports
            let ::core::option::Option::Some((segment, rest)) = path.split_first() else {
                return ::core::result::Result::Err(::scry::__private::needs_interior_mutability(self.type_name()));
            };
            let type_name = self.type_name();
            match self {
                #(#write_shared_arms)*
            }
        }

        /// Switches to a unit variant named by a JSON string. A data-carrying variant cannot be
        /// built from JSON; change its fields by path instead.
        fn assign(&mut self, value: &::scry::Value) -> ::core::result::Result<(), ::scry::Error> {
            match value.as_str() {
                #(::core::option::Option::Some(#unit_names) => {
                    *self = Self::#unit_idents;
                    ::core::result::Result::Ok(())
                })*
                _ => ::core::result::Result::Err(::scry::__private::mismatch(#expected, value)),
            }
        }
    }
}
