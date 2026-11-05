//! Support for generated code: the derive, call shims, hook points. Not a public API.
//!
//! Generated code handles values of any type through the probe macros (`probe_ref!` and friends),
//! which pick how at compile time; see [`crate::probes`]. The choice is made at the value's own
//! location, so it sees the concrete type. Inside a generic type the derive adds `T: Reflect` to
//! every type parameter, so fields built from those parameters resolve to `Reflect` as well.

use std::any::type_name;
use std::fmt::Debug;

pub use serde_json::Map;
use serde_json::Value;

use crate::{Error, Kind, Reflect, Registry, Segment, Visit, VisitMut};

#[cfg(feature = "dioxus")]
pub use crate::dioxus_hooks as dioxus;
pub use crate::endpoint::start as start_endpoint;
pub use crate::run_async::{block_on, capture_runtime};

/// Maximum length of a `Debug` rendering in a snapshot, so one large opaque value cannot swamp it.
const MAX_DEBUG_LEN: usize = 1_000;

pub use crate::probes::{Probe, ProbeMut};
pub use crate::{probe_arg, probe_mut, probe_ref, probe_ref_arg};
pub use scry_rs_derive::peeler_for;

/// The owning counterpart of [`Probe`], for handles such as signals that a registry keeps a copy
/// of: a navigable clone, a `Debug`-only clone, or just the type name.
pub struct ProbeOwned<'a, T>(pub &'a T);

pub trait ViaReflectOwned {
    fn scry_owned(&self) -> Box<dyn Reflect>;
}

impl<T: Reflect + Clone + 'static> ViaReflectOwned for &&ProbeOwned<'_, T> {
    fn scry_owned(&self) -> Box<dyn Reflect> {
        Box::new(self.0.clone())
    }
}

pub trait ViaDebugOwned {
    fn scry_owned(&self) -> Box<dyn Reflect>;
}

impl<T: Debug + Clone + 'static> ViaDebugOwned for &ProbeOwned<'_, T> {
    fn scry_owned(&self) -> Box<dyn Reflect> {
        Box::new(DebugOwned { value: Box::new(self.0.clone()), type_name: type_name::<T>() })
    }
}

pub trait ViaOpaqueOwned {
    fn scry_owned(&self) -> Box<dyn Reflect>;
}

impl<T> ViaOpaqueOwned for ProbeOwned<'_, T> {
    fn scry_owned(&self) -> Box<dyn Reflect> {
        Box::new(OpaqueValue { type_name: type_name::<T>() })
    }
}

pub use crate::call::{Function, Receiver, This};
pub use crate::from_value::{FromValue, SelfProbe, ViaClone, ViaNoClone};
pub use crate::probes::{ArgProbe, RefProbe, Unconstructible};

pub use crate::hooks::{CallGuard, Capture, HookSite};

/// Runs a hooked function's body. Taking it as `FnOnce` makes the closure one, so the body can
/// return what it reborrows from its captured parameters (`&mut self.field`), as the function
/// itself could.
#[inline(always)]
pub fn run_once<R>(body: impl FnOnce() -> R) -> R {
    body()
}

/// Every function the generator compiled a hook point into, from every crate.
#[linkme::distributed_slice]
pub static HOOK_SITES: [&'static HookSite];

/// Every function the generator made callable, from every crate.
#[linkme::distributed_slice]
pub static FUNCTIONS: [Function];

/// Registers one `static` with a registry. The generator emits one per static, next to it, into
/// `STATICS`; the linker gathers them from every crate into one list.
pub type StaticEntry = fn(&mut Registry);

#[linkme::distributed_slice]
pub static STATICS: [StaticEntry];

pub use linkme;

/// `impl Reflect for dyn Trait`, emitted by the generator for each trait it can make object-safe
/// reflection for. Every call goes through the hidden `__scry_ref`/`__scry_mut` methods the
/// generator adds to the trait, which each implementing type overrides with its own probe, so a
/// `Box<dyn Trait>` reflects the concrete value behind it.
#[macro_export]
#[doc(hidden)]
macro_rules! __reflect_dyn {
    ([$($generics:tt)*] [$($trait_path:tt)*] [$($self_ty:tt)*] [$($where:tt)*]) => {
        #[automatically_derived]
        impl<$($generics)*> $crate::Reflect for $($self_ty)* where $($where)* {
            fn type_name(&self) -> &'static str {
                <Self as $($trait_path)*>::__scry_ref(self).get().type_name()
            }

            fn kind(&self) -> $crate::Kind {
                <Self as $($trait_path)*>::__scry_ref(self).get().kind()
            }

            fn members(&self) -> ::std::vec::Vec<$crate::Member> {
                <Self as $($trait_path)*>::__scry_ref(self).get().members()
            }

            fn member_count(&self) -> usize {
                <Self as $($trait_path)*>::__scry_ref(self).get().member_count()
            }

            fn snapshot(&self, depth: usize) -> $crate::Value {
                <Self as $($trait_path)*>::__scry_ref(self).snapshot(depth)
            }

            fn read(&self, path: &[$crate::Segment], visit: $crate::Visit<'_>) -> ::core::result::Result<(), $crate::Error> {
                <Self as $($trait_path)*>::__scry_ref(self).read(path, visit)
            }

            fn write(&mut self, path: &[$crate::Segment], visit: $crate::VisitMut<'_>) -> ::core::result::Result<(), $crate::Error> {
                <Self as $($trait_path)*>::__scry_mut(self).write(path, visit)
            }

            fn write_shared(&self, path: &[$crate::Segment], visit: $crate::VisitMut<'_>) -> ::core::result::Result<(), $crate::Error> {
                <Self as $($trait_path)*>::__scry_ref(self).write_shared(path, visit)
            }

            fn assign(&mut self, value: &$crate::Value) -> ::core::result::Result<(), $crate::Error> {
                <Self as $($trait_path)*>::__scry_mut(self).assign(value)
            }
        }
    };
}

pub use crate::__reflect_dyn as reflect_dyn;

/// A field viewed for reading, through whatever its type supports.
pub enum FieldRef<'a> {
    Reflect(&'a dyn Reflect),
    /// A view made for the occasion: a third-party collection, a serialized value.
    Owned(Box<dyn Reflect + 'a>),
    Debug(DebugValue<'a>),
    Opaque(OpaqueValue),
}

impl FieldRef<'_> {
    /// What a trait method's default body reports for a type the generator never saw.
    pub fn opaque(type_name: &'static str) -> Self {
        FieldRef::Opaque(OpaqueValue { type_name })
    }

    pub fn get(&self) -> &dyn Reflect {
        match self {
            FieldRef::Reflect(value) => *value,
            FieldRef::Owned(value) => value.as_ref(),
            FieldRef::Debug(value) => value,
            FieldRef::Opaque(value) => value,
        }
    }

    pub fn snapshot(&self, depth: usize) -> Value {
        match self {
            FieldRef::Reflect(value) => value.snapshot(depth),
            FieldRef::Owned(value) => value.snapshot(depth),
            FieldRef::Debug(value) => value.snapshot(depth),
            FieldRef::Opaque(value) => value.snapshot(depth),
        }
    }

    pub fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        match self {
            FieldRef::Reflect(value) => value.read(path, visit),
            FieldRef::Owned(value) => value.read(path, visit),
            FieldRef::Debug(value) => value.read(path, visit),
            FieldRef::Opaque(value) => value.read(path, visit),
        }
    }

    pub fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        match self {
            FieldRef::Reflect(value) => value.write_shared(path, visit),
            FieldRef::Owned(value) => value.write_shared(path, visit),
            FieldRef::Debug(value) => Err(does_not_reflect(value.type_name)),
            FieldRef::Opaque(value) => Err(does_not_reflect(value.type_name)),
        }
    }
}

/// A field viewed for writing: navigable, written through a view (serde, `FromStr`), or a type
/// that cannot be written at all.
pub enum FieldMut<'a> {
    Reflect(&'a mut dyn Reflect),
    Owned(Box<dyn Reflect + 'a>),
    Other(&'static str),
}

impl FieldMut<'_> {
    pub fn write(self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        match self {
            FieldMut::Reflect(value) => value.write(path, visit),
            FieldMut::Owned(mut value) => value.write(path, visit),
            FieldMut::Other(type_name) => Err(does_not_reflect(type_name)),
        }
    }

    pub fn assign(self, value: &Value) -> Result<(), Error> {
        match self {
            FieldMut::Reflect(target) => target.assign(value),
            FieldMut::Owned(mut target) => target.assign(value),
            FieldMut::Other(type_name) => Err(does_not_reflect(type_name)),
        }
    }
}

/// A value shown through its `Debug` output: readable, never navigable or writable.
pub struct DebugValue<'a> {
    value: &'a dyn Debug,
    type_name: &'static str,
}

impl<'a> DebugValue<'a> {
    pub(crate) fn new(value: &'a dyn Debug, type_name: &'static str) -> Self {
        Self { value, type_name }
    }
}

impl Reflect for DebugValue<'_> {
    fn type_name(&self) -> &'static str {
        self.type_name
    }

    fn kind(&self) -> Kind {
        Kind::Opaque
    }

    fn snapshot(&self, _depth: usize) -> Value {
        let mut text = format!("{:?}", self.value);
        if text.len() > MAX_DEBUG_LEN {
            let cut = (0..=MAX_DEBUG_LEN).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
            text.truncate(cut);
            text.push('…');
        }
        Value::String(text)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { Err(Error::NotNavigable { type_name: self.type_name }) }
    }

    fn write(&mut self, _path: &[Segment], _visit: VisitMut<'_>) -> Result<(), Error> {
        Err(does_not_reflect(self.type_name))
    }
}

/// An owned copy of a value shown through its `Debug` output.
struct DebugOwned {
    value: Box<dyn Debug>,
    type_name: &'static str,
}

impl Reflect for DebugOwned {
    fn type_name(&self) -> &'static str {
        self.type_name
    }

    fn kind(&self) -> Kind {
        Kind::Opaque
    }

    fn snapshot(&self, depth: usize) -> Value {
        DebugValue { value: &*self.value, type_name: self.type_name }.snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { Err(Error::NotNavigable { type_name: self.type_name }) }
    }

    fn write(&mut self, _path: &[Segment], _visit: VisitMut<'_>) -> Result<(), Error> {
        Err(does_not_reflect(self.type_name))
    }
}

/// A value that implements neither `Reflect` nor `Debug`: only its type name is known.
pub struct OpaqueValue {
    type_name: &'static str,
}

impl OpaqueValue {
    pub(crate) fn new(type_name: &'static str) -> Self {
        Self { type_name }
    }
}

impl Reflect for OpaqueValue {
    fn type_name(&self) -> &'static str {
        self.type_name
    }

    fn kind(&self) -> Kind {
        Kind::Opaque
    }

    fn snapshot(&self, _depth: usize) -> Value {
        truncated(self.type_name)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { Err(Error::NotNavigable { type_name: self.type_name }) }
    }

    fn write(&mut self, _path: &[Segment], _visit: VisitMut<'_>) -> Result<(), Error> {
        Err(does_not_reflect(self.type_name))
    }
}

pub fn truncated(type_name: &str) -> Value {
    Value::String(format!("<{}>", crate::reflect::short_type_name(type_name)))
}

pub fn no_member(type_name: &'static str, segment: &Segment) -> Error {
    Error::NoSuchMember { type_name, member: segment.to_string() }
}

pub fn mismatch(expected: &'static str, found: &Value) -> Error {
    Error::mismatch(expected, found)
}

pub fn not_writable(reason: &str) -> Error {
    Error::not_writable(reason)
}

pub fn needs_interior_mutability(type_name: &'static str) -> Error {
    Error::needs_interior_mutability(type_name)
}

fn does_not_reflect(type_name: &'static str) -> Error {
    Error::not_writable(format!("{type_name} does not implement Reflect"))
}
