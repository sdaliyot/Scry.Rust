//! How generated code treats a value of a type it knows nothing about.
//!
//! Generated code - the derive, call shims, hook points - meets values of every type: the app's own,
//! std's, and third-party ones that implement nothing of Scry's. Each use goes through one of the
//! macros below ([`probe_ref!`](crate::probe_ref), [`probe_mut!`](crate::probe_mut),
//! [`probe_arg!`](crate::probe_arg), [`probe_ref_arg!`](crate::probe_ref_arg)), which expand at the
//! use site and pick, at compile time and for the concrete type there, the first way of handling
//! it whose bounds hold. That is autoref specialization: the probe is borrowed once per tier, and
//! method resolution tries the impl that strips the most borrows first.
//!
//! Reading a value, best first:
//!
//! 1. `Reflect`: fully navigable and writable.
//! 2. `Deref` to a `Reflect` target (`Pin<Box<T>>`, most wrappers): seen through.
//! 3. A lock or cell (a [`Wrapper`]) around a serde value - `Mutex<Foreign>`,
//!    `Arc<RwLock<HashMap<String, Foreign>>>`: read under the lock as JSON, and written under it
//!    by round-tripping through JSON. Rust picks impls without looking inside generic types, so a
//!    `Mutex<T>` is only `Reflect` when `T` is; this tier is what reaches a foreign `T` in one.
//! 4. Iterable by reference, with `Reflect` items or `(key, Reflect)` pairs (third-party
//!    collections): a sequence or map whose items stay navigable, writable through their own
//!    locks, and callable.
//! 5. `serde::Serialize` (`chrono`, `uuid`, `url`, `rust_decimal`, ...): a structured JSON view.
//! 6. `Debug`: its text.
//! 7. Anything else: its type name.
//!
//! Writing one: `Reflect`; `DerefMut` to a `Reflect` target; `Serialize + Deserialize`, by
//! round-tripping the value through JSON (so `set at.date 2026-01-01` and `set at.offset.hours 2`
//! both work); `FromStr`, from a JSON string; else refused. Building one from JSON, for a call or
//! a hook: `FromValue`, then `Deserialize`, then `FromStr`.
//!
//! None of this names a crate: any type that implements these std and serde traits is covered,
//! including ones written after this was.

use std::any::type_name;
use std::borrow::Borrow;
use std::cell::Cell;
use std::convert::Infallible;
use std::fmt::{Debug, Display};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::__private::{DebugValue, FieldMut, FieldRef, OpaqueValue, truncated};
use crate::from_value::FromValue;
use crate::{Error, Kind, Member, Reflect, Segment, Visit, VisitMut};

/// The most elements a third-party collection lists.
const MAX_ELEMENTS: usize = 100;

/// `probe_ref!(&value)`: a [`FieldRef`] for reading `value`.
#[macro_export]
#[doc(hidden)]
macro_rules! probe_ref {
    ($value:expr) => {{
        #[allow(unused_imports)]
        use $crate::probes::read::*;
        (&&&&&&&$crate::probes::Probe($value)).scry_ref()
    }};
}

/// `probe_mut!(&mut value)`: a [`FieldMut`] for writing `value`.
#[macro_export]
#[doc(hidden)]
macro_rules! probe_mut {
    ($value:expr) => {{
        #[allow(unused_imports)]
        use $crate::probes::write::*;
        (&&&&&&$crate::probes::ProbeMut::new($value)).scry_mut()
    }};
}

/// `probe_arg!(Type, &json)`: a `Type` built from JSON, or why it cannot be.
#[macro_export]
#[doc(hidden)]
macro_rules! probe_arg {
    ($ty:ty, $value:expr) => {{
        #[allow(unused_imports)]
        use $crate::probes::build::*;
        (&&&&$crate::probes::ArgProbe::<$ty>::new()).scry_arg($value)
    }};
}

/// `probe_ref_arg!(Type, &json)`: something that lends a `&Type` built from JSON (through
/// `Borrow`), or why it cannot be built. `Type` may be unsized: `str`, `[u8]`, `Path`.
#[macro_export]
#[doc(hidden)]
macro_rules! probe_ref_arg {
    ($ty:ty, $value:expr) => {{
        #[allow(unused_imports)]
        use $crate::probes::lend::*;
        (&&&&&$crate::probes::RefProbe::<$ty>::new()).scry_ref_arg($value)
    }};
}

// --- Reading ------------------------------------------------------------------------------------

pub struct Probe<'a, T>(pub &'a T);

/// The reading tiers, as traits for the macro to import.
pub mod read {
    use super::*;

    pub trait ViaReflect<'a> {
        fn scry_ref(&self) -> FieldRef<'a>;
    }

    impl<'a, T: Reflect> ViaReflect<'a> for &&&&&&Probe<'a, T> {
        fn scry_ref(&self) -> FieldRef<'a> {
            FieldRef::Reflect(self.0)
        }
    }

    pub trait ViaDeref<'a> {
        fn scry_ref(&self) -> FieldRef<'a>;
    }

    impl<'a, T: Deref> ViaDeref<'a> for &&&&&Probe<'a, T>
    where
        T::Target: Reflect + Sized,
    {
        fn scry_ref(&self) -> FieldRef<'a> {
            FieldRef::Reflect(&**self.0)
        }
    }

    pub trait ViaWrappedSerde<'a> {
        fn scry_ref(&self) -> FieldRef<'a>;
    }

    impl<'a, W: Wrapper> ViaWrappedSerde<'a> for &&&&Probe<'a, W>
    where
        W::Inner: Serialize + DeserializeOwned,
    {
        fn scry_ref(&self) -> FieldRef<'a> {
            FieldRef::Owned(Box::new(WrappedSerde { wrapper: self.0 }))
        }
    }

    pub trait ViaIter<'a> {
        fn scry_ref(&self) -> FieldRef<'a>;
    }

    impl<'a, T> ViaIter<'a> for &&&Probe<'a, T>
    where
        &'a T: IntoIterator,
        <&'a T as IntoIterator>::Item: Element<'a>,
    {
        fn scry_ref(&self) -> FieldRef<'a> {
            FieldRef::Owned(Box::new(IterView { items: self.0, type_name: type_name::<T>() }))
        }
    }

    pub trait ViaSerialize<'a> {
        fn scry_ref(&self) -> FieldRef<'a>;
    }

    impl<'a, T: Serialize> ViaSerialize<'a> for &&Probe<'a, T> {
        fn scry_ref(&self) -> FieldRef<'a> {
            FieldRef::Owned(Box::new(SerdeView::of(self.0)))
        }
    }

    pub trait ViaDebug<'a> {
        fn scry_ref(&self) -> FieldRef<'a>;
    }

    impl<'a, T: Debug> ViaDebug<'a> for &Probe<'a, T> {
        fn scry_ref(&self) -> FieldRef<'a> {
            FieldRef::Debug(DebugValue::new(self.0, type_name::<T>()))
        }
    }

    pub trait ViaOpaque<'a> {
        fn scry_ref(&self) -> FieldRef<'a>;
    }

    impl<'a, T> ViaOpaque<'a> for Probe<'a, T> {
        fn scry_ref(&self) -> FieldRef<'a> {
            FieldRef::Opaque(OpaqueValue::new(type_name::<T>()))
        }
    }
}

/// One item of an iterable collection: an element, or a key and its value.
pub trait Element<'a> {
    fn entry(self) -> (Option<String>, &'a dyn Reflect);
}

impl<'a, V: Reflect> Element<'a> for &'a V {
    fn entry(self) -> (Option<String>, &'a dyn Reflect) {
        (None, self)
    }
}

impl<'a, K: Display, V: Reflect> Element<'a> for (&'a K, &'a V) {
    fn entry(self) -> (Option<String>, &'a dyn Reflect) {
        (Some(self.0.to_string()), self.1)
    }
}

/// A collection Scry knows only as iterable: a sequence by position (`[2]`), or a map by its keys'
/// `Display` form. Read-only itself, but its items are reached as themselves, so a lock or signal
/// inside one is still written through.
struct IterView<'a, T> {
    items: &'a T,
    type_name: &'static str,
}

impl<'a, T> IterView<'a, T>
where
    &'a T: IntoIterator,
    <&'a T as IntoIterator>::Item: Element<'a>,
{
    fn entries(&self) -> impl Iterator<Item = (Option<String>, &'a dyn Reflect)> {
        self.items.into_iter().map(Element::entry)
    }

    fn keyed(&self) -> bool {
        self.entries().next().is_some_and(|(key, _)| key.is_some())
    }

    fn find(&self, segment: &Segment) -> Result<&'a dyn Reflect, Error> {
        let found = if self.keyed() {
            let name = segment.name();
            self.entries().find(|(key, _)| key.as_deref() == Some(&*name))
        } else {
            segment.index().and_then(|index| self.entries().nth(index))
        };
        found.map(|(_, value)| value).ok_or_else(|| crate::__private::no_member(self.type_name, segment))
    }
}

impl<'a, T> Reflect for IterView<'a, T>
where
    &'a T: IntoIterator,
    <&'a T as IntoIterator>::Item: Element<'a>,
{
    fn type_name(&self) -> &'static str {
        self.type_name
    }

    fn kind(&self) -> Kind {
        if self.keyed() { Kind::Struct } else { Kind::Sequence }
    }

    fn members(&self) -> Vec<Member> {
        self.entries()
            .take(MAX_ELEMENTS)
            .enumerate()
            .map(|(index, (key, value))| Member::new(key.unwrap_or_else(|| format!("[{index}]")), value.type_name()))
            .collect()
    }

    fn member_count(&self) -> usize {
        self.entries().count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        if depth == 0 {
            return truncated(self.type_name);
        }
        if self.keyed() {
            let fields = self
                .entries()
                .take(MAX_ELEMENTS)
                .map(|(key, value)| (key.unwrap_or_default(), value.snapshot(depth - 1)));
            Value::Object(fields.collect())
        } else {
            Value::Array(self.entries().take(MAX_ELEMENTS).map(|(_, value)| value.snapshot(depth - 1)).collect())
        }
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        match path.split_first() {
            None => visit(self),
            Some((segment, rest)) => self.find(segment)?.read(rest, visit),
        }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        match path.split_first() {
            None => Err(Error::needs_interior_mutability(self.type_name)),
            Some((segment, rest)) => self.find(segment)?.write_shared(rest, visit),
        }
    }
}

/// A value seen through `serde::Serialize`: its JSON form, navigable but read-only (writes go
/// through [`SerdeMut`]), under the value's own type name.
pub(crate) struct SerdeView {
    json: Value,
    type_name: &'static str,
}

impl SerdeView {
    fn of<T: Serialize>(value: &T) -> Self {
        let json = serde_json::to_value(value)
            .unwrap_or_else(|error| Value::String(format!("<serialization failed: {error}>")));
        Self { json, type_name: type_name::<T>() }
    }
}

impl Reflect for SerdeView {
    fn type_name(&self) -> &'static str {
        self.type_name
    }

    fn kind(&self) -> Kind {
        self.json.kind()
    }

    fn members(&self) -> Vec<Member> {
        self.json.members()
    }

    fn member_count(&self) -> usize {
        self.json.member_count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        self.json.snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { self.json.read(path, visit) }
    }

    fn write(&mut self, _path: &[Segment], _visit: VisitMut<'_>) -> Result<(), Error> {
        Err(Error::not_writable(format!("{} is a serialized copy", self.type_name)))
    }
}

/// A lock or cell whatever it holds: how to read and write the value inside under its own
/// locking. `Reflect` impls of locks need `T: Reflect`; this does not, so a lock around a foreign
/// serde value can still be reached (see [`read::ViaWrappedSerde`]). Smart pointers pass it on:
/// `Arc<Mutex<T>>` is a wrapper of `T`.
pub trait Wrapper {
    type Inner;
    fn read_inner(&self, f: &mut dyn FnMut(&Self::Inner)) -> Result<(), Error>;
    fn write_inner(&self, f: &mut dyn FnMut(&mut Self::Inner)) -> Result<(), Error>;
}

impl<T> Wrapper for std::sync::Mutex<T> {
    type Inner = T;
    fn read_inner(&self, f: &mut dyn FnMut(&T)) -> Result<(), Error> {
        f(&**crate::impls::acquire(type_name::<Self>(), || self.try_lock(), &|| self.clear_poison())?);
        Ok(())
    }
    fn write_inner(&self, f: &mut dyn FnMut(&mut T)) -> Result<(), Error> {
        f(&mut **crate::impls::acquire(type_name::<Self>(), || self.try_lock(), &|| self.clear_poison())?);
        Ok(())
    }
}

impl<T> Wrapper for std::sync::RwLock<T> {
    type Inner = T;
    fn read_inner(&self, f: &mut dyn FnMut(&T)) -> Result<(), Error> {
        f(&**crate::impls::acquire(type_name::<Self>(), || self.try_read(), &|| self.clear_poison())?);
        Ok(())
    }
    fn write_inner(&self, f: &mut dyn FnMut(&mut T)) -> Result<(), Error> {
        f(&mut **crate::impls::acquire(type_name::<Self>(), || self.try_write(), &|| self.clear_poison())?);
        Ok(())
    }
}

impl<T> Wrapper for std::cell::RefCell<T> {
    type Inner = T;
    fn read_inner(&self, f: &mut dyn FnMut(&T)) -> Result<(), Error> {
        f(&*self.try_borrow().map_err(|_| Error::LockBusy { type_name: type_name::<Self>() })?);
        Ok(())
    }
    fn write_inner(&self, f: &mut dyn FnMut(&mut T)) -> Result<(), Error> {
        f(&mut *self.try_borrow_mut().map_err(|_| Error::LockBusy { type_name: type_name::<Self>() })?);
        Ok(())
    }
}

#[cfg(feature = "lock_api")]
impl<R: lock_api::RawMutex, T> Wrapper for lock_api::Mutex<R, T> {
    type Inner = T;
    fn read_inner(&self, f: &mut dyn FnMut(&T)) -> Result<(), Error> {
        f(&*crate::impls::wait_for(type_name::<Self>(), || self.try_lock())?);
        Ok(())
    }
    fn write_inner(&self, f: &mut dyn FnMut(&mut T)) -> Result<(), Error> {
        f(&mut *crate::impls::wait_for(type_name::<Self>(), || self.try_lock())?);
        Ok(())
    }
}

#[cfg(feature = "lock_api")]
impl<R: lock_api::RawRwLock, T> Wrapper for lock_api::RwLock<R, T> {
    type Inner = T;
    fn read_inner(&self, f: &mut dyn FnMut(&T)) -> Result<(), Error> {
        f(&*crate::impls::wait_for(type_name::<Self>(), || self.try_read())?);
        Ok(())
    }
    fn write_inner(&self, f: &mut dyn FnMut(&mut T)) -> Result<(), Error> {
        f(&mut *crate::impls::wait_for(type_name::<Self>(), || self.try_write())?);
        Ok(())
    }
}

#[cfg(feature = "tokio")]
impl<T> Wrapper for tokio::sync::Mutex<T> {
    type Inner = T;
    fn read_inner(&self, f: &mut dyn FnMut(&T)) -> Result<(), Error> {
        f(&*crate::impls::wait_for(type_name::<Self>(), || self.try_lock().ok())?);
        Ok(())
    }
    fn write_inner(&self, f: &mut dyn FnMut(&mut T)) -> Result<(), Error> {
        f(&mut *crate::impls::wait_for(type_name::<Self>(), || self.try_lock().ok())?);
        Ok(())
    }
}

#[cfg(feature = "tokio")]
impl<T> Wrapper for tokio::sync::RwLock<T> {
    type Inner = T;
    fn read_inner(&self, f: &mut dyn FnMut(&T)) -> Result<(), Error> {
        f(&*crate::impls::wait_for(type_name::<Self>(), || self.try_read().ok())?);
        Ok(())
    }
    fn write_inner(&self, f: &mut dyn FnMut(&mut T)) -> Result<(), Error> {
        f(&mut *crate::impls::wait_for(type_name::<Self>(), || self.try_write().ok())?);
        Ok(())
    }
}

macro_rules! pointer_wrapper {
    ($($pointer:ident)::+) => {
        impl<W: Wrapper> Wrapper for $($pointer)::+<W> {
            type Inner = W::Inner;
            fn read_inner(&self, f: &mut dyn FnMut(&W::Inner)) -> Result<(), Error> {
                (**self).read_inner(f)
            }
            fn write_inner(&self, f: &mut dyn FnMut(&mut W::Inner)) -> Result<(), Error> {
                (**self).write_inner(f)
            }
        }
    };
}

pointer_wrapper!(std::sync::Arc);
pointer_wrapper!(std::rc::Rc);
pointer_wrapper!(Box);

/// A serde value inside a [`Wrapper`], reached under its lock.
struct WrappedSerde<'a, W> {
    wrapper: &'a W,
}

impl<W: Wrapper> WrappedSerde<'_, W>
where
    W::Inner: Serialize + DeserializeOwned,
{
    fn view(&self) -> Result<SerdeView, Error> {
        let mut view = None;
        self.wrapper.read_inner(&mut |value| view = Some(SerdeView::of(value)))?;
        view.ok_or_else(|| Error::LockBusy { type_name: type_name::<W>() })
    }
}

impl<W: Wrapper> Reflect for WrappedSerde<'_, W>
where
    W::Inner: Serialize + DeserializeOwned,
{
    fn type_name(&self) -> &'static str {
        type_name::<W>()
    }

    fn kind(&self) -> Kind {
        self.view().map_or(Kind::Opaque, |view| view.kind())
    }

    fn members(&self) -> Vec<Member> {
        self.view().map(|view| view.members()).unwrap_or_default()
    }

    fn member_count(&self) -> usize {
        self.view().map_or(0, |view| view.member_count())
    }

    fn snapshot(&self, depth: usize) -> Value {
        match self.view() {
            Ok(view) => view.snapshot(depth),
            Err(_) => Value::String(format!("<locked: {}>", crate::reflect::short_type_name(type_name::<W>()))),
        }
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        self.view()?.read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let mut result = Ok(());
        self.wrapper.write_inner(&mut |value| result = SerdeMut { value }.write(path, visit))?;
        result
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        let mut result = Ok(());
        self.wrapper.write_inner(&mut |inner| result = SerdeMut { value: inner }.assign(value))?;
        result
    }
}

// --- Writing ------------------------------------------------------------------------------------

/// The mutable counterpart of [`Probe`]. Method resolution needs `&self` receivers, so the
/// exclusive reference sits in a `Cell` and is taken out exactly once.
pub struct ProbeMut<'a, T>(Cell<Option<&'a mut T>>);

impl<'a, T> ProbeMut<'a, T> {
    pub fn new(value: &'a mut T) -> Self {
        Self(Cell::new(Some(value)))
    }

    fn take(&self) -> &'a mut T {
        self.0.take().expect("a ProbeMut is resolved once")
    }
}

/// The writing tiers, as traits for the macro to import.
pub mod write {
    use super::*;

    pub trait ViaReflectMut<'a> {
        fn scry_mut(&self) -> FieldMut<'a>;
    }

    impl<'a, T: Reflect> ViaReflectMut<'a> for &&&&&ProbeMut<'a, T> {
        fn scry_mut(&self) -> FieldMut<'a> {
            FieldMut::Reflect(self.take())
        }
    }

    pub trait ViaDerefMut<'a> {
        fn scry_mut(&self) -> FieldMut<'a>;
    }

    impl<'a, T: DerefMut> ViaDerefMut<'a> for &&&&ProbeMut<'a, T>
    where
        T::Target: Reflect + Sized,
    {
        fn scry_mut(&self) -> FieldMut<'a> {
            FieldMut::Reflect(&mut **self.take())
        }
    }

    pub trait ViaWrappedSerdeMut<'a> {
        fn scry_mut(&self) -> FieldMut<'a>;
    }

    impl<'a, W: Wrapper> ViaWrappedSerdeMut<'a> for &&&ProbeMut<'a, W>
    where
        W::Inner: Serialize + DeserializeOwned,
    {
        fn scry_mut(&self) -> FieldMut<'a> {
            let wrapper: &'a W = self.take();
            FieldMut::Owned(Box::new(WrappedSerde { wrapper }))
        }
    }

    pub trait ViaSerdeMut<'a> {
        fn scry_mut(&self) -> FieldMut<'a>;
    }

    impl<'a, T: Serialize + DeserializeOwned> ViaSerdeMut<'a> for &&ProbeMut<'a, T> {
        fn scry_mut(&self) -> FieldMut<'a> {
            FieldMut::Owned(Box::new(SerdeMut { value: self.take() }))
        }
    }

    pub trait ViaFromStrMut<'a> {
        fn scry_mut(&self) -> FieldMut<'a>;
    }

    impl<'a, T: FromStr + 'a> ViaFromStrMut<'a> for &ProbeMut<'a, T>
    where
        T::Err: Display,
    {
        fn scry_mut(&self) -> FieldMut<'a> {
            FieldMut::Owned(Box::new(FromStrMut { value: self.take() }))
        }
    }

    pub trait ViaOtherMut<'a> {
        fn scry_mut(&self) -> FieldMut<'a>;
    }

    impl<'a, T> ViaOtherMut<'a> for ProbeMut<'a, T> {
        fn scry_mut(&self) -> FieldMut<'a> {
            FieldMut::Other(type_name::<T>())
        }
    }
}

/// A value written through serde: its JSON form is edited, then deserialized back in its place.
/// The value only changes if the edited JSON deserializes.
struct SerdeMut<'a, T> {
    value: &'a mut T,
}

impl<T: Serialize + DeserializeOwned> Reflect for SerdeMut<'_, T> {
    fn type_name(&self) -> &'static str {
        type_name::<T>()
    }

    fn kind(&self) -> Kind {
        SerdeView::of(&*self.value).kind()
    }

    fn snapshot(&self, depth: usize) -> Value {
        SerdeView::of(&*self.value).snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        SerdeView::of(&*self.value).read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let mut json = serde_json::to_value(&*self.value).map_err(|error| Error::not_writable(error.to_string()))?;
        json.write(path, visit)?;
        *self.value = deserialize(&json)?;
        Ok(())
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        *self.value = deserialize(value)?;
        Ok(())
    }
}

fn deserialize<T: DeserializeOwned>(value: &Value) -> Result<T, Error> {
    T::deserialize(value)
        .map_err(|error| Error::TypeMismatch { expected: type_name::<T>(), found: format!("{value} ({error})") })
}

/// A value written from a JSON string through `FromStr`.
struct FromStrMut<'a, T> {
    value: &'a mut T,
}

impl<T: FromStr> Reflect for FromStrMut<'_, T>
where
    T::Err: Display,
{
    fn type_name(&self) -> &'static str {
        type_name::<T>()
    }

    fn kind(&self) -> Kind {
        Kind::Scalar
    }

    fn snapshot(&self, _depth: usize) -> Value {
        truncated(type_name::<T>())
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { Err(Error::NotNavigable { type_name: type_name::<T>() }) }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { Err(Error::NotNavigable { type_name: type_name::<T>() }) }
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        *self.value = from_str(value)?;
        Ok(())
    }
}

/// A `FromStr` value from a JSON string, or from a number or bool written as one.
fn from_str<T: FromStr>(value: &Value) -> Result<T, Error>
where
    T::Err: Display,
{
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(_) | Value::Bool(_) => value.to_string(),
        _ => return Err(Error::mismatch("a string", value)),
    };
    text.parse().map_err(|error: T::Err| Error::TypeMismatch {
        expected: type_name::<T>(),
        found: format!("{value} ({error})"),
    })
}

// --- Building from JSON -------------------------------------------------------------------------

/// Picks how a by-value argument of type `T` is built.
pub struct ArgProbe<T>(PhantomData<fn() -> T>);

impl<T> ArgProbe<T> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

/// The building tiers, as traits for the macro to import.
pub mod build {
    use super::*;

    pub trait ViaFromValue<T> {
        fn scry_arg(&self, value: &Value) -> Result<T, Error>;
    }

    impl<T: FromValue> ViaFromValue<T> for &&&ArgProbe<T> {
        fn scry_arg(&self, value: &Value) -> Result<T, Error> {
            T::from_value(value)
        }
    }

    pub trait ViaDeserialize<T> {
        fn scry_arg(&self, value: &Value) -> Result<T, Error>;
    }

    impl<T: DeserializeOwned> ViaDeserialize<T> for &&ArgProbe<T> {
        fn scry_arg(&self, value: &Value) -> Result<T, Error> {
            deserialize(value)
        }
    }

    pub trait ViaFromStr<T> {
        fn scry_arg(&self, value: &Value) -> Result<T, Error>;
    }

    impl<T: FromStr> ViaFromStr<T> for &ArgProbe<T>
    where
        T::Err: Display,
    {
        fn scry_arg(&self, value: &Value) -> Result<T, Error> {
            from_str(value)
        }
    }

    pub trait ViaNoValue<T> {
        fn scry_arg(&self, value: &Value) -> Result<T, Error>;
    }

    impl<T> ViaNoValue<T> for ArgProbe<T> {
        fn scry_arg(&self, _value: &Value) -> Result<T, Error> {
            Err(Error::CannotConvert { type_name: type_name::<T>() })
        }
    }
}

/// Picks how a `&T` argument is built: a `T` when one can be built, else its owned form for an
/// unsized `T` (`String` for `str`, `Vec<U>` for `[U]`, `PathBuf` for `Path`). The shim lends the
/// result to the call through `Borrow<T>`.
pub struct RefProbe<T: ?Sized>(PhantomData<fn() -> Box<T>>);

impl<T: ?Sized> RefProbe<T> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

/// The lending tiers, as traits for the macro to import.
pub mod lend {
    use super::*;

    pub trait ViaRefValue {
        type Held;
        fn scry_ref_arg(&self, value: &Value) -> Result<Self::Held, Error>;
    }

    impl<T: FromValue> ViaRefValue for &&&&RefProbe<T> {
        type Held = T;
        fn scry_ref_arg(&self, value: &Value) -> Result<T, Error> {
            T::from_value(value)
        }
    }

    pub trait ViaRefDeserialize {
        type Held;
        fn scry_ref_arg(&self, value: &Value) -> Result<Self::Held, Error>;
    }

    impl<T: DeserializeOwned> ViaRefDeserialize for &&&RefProbe<T> {
        type Held = T;
        fn scry_ref_arg(&self, value: &Value) -> Result<T, Error> {
            deserialize(value)
        }
    }

    pub trait ViaRefOwned {
        type Held;
        fn scry_ref_arg(&self, value: &Value) -> Result<Self::Held, Error>;
    }

    impl<T: ?Sized + ToOwned> ViaRefOwned for &&RefProbe<T>
    where
        T::Owned: FromValue,
    {
        type Held = T::Owned;
        fn scry_ref_arg(&self, value: &Value) -> Result<T::Owned, Error> {
            T::Owned::from_value(value)
        }
    }

    pub trait ViaRefOwnedDeserialize {
        type Held;
        fn scry_ref_arg(&self, value: &Value) -> Result<Self::Held, Error>;
    }

    impl<T: ?Sized + ToOwned> ViaRefOwnedDeserialize for &RefProbe<T>
    where
        T::Owned: DeserializeOwned,
    {
        type Held = T::Owned;
        fn scry_ref_arg(&self, value: &Value) -> Result<T::Owned, Error> {
            deserialize(value)
        }
    }

    pub trait ViaRefNone {
        type Held;
        fn scry_ref_arg(&self, value: &Value) -> Result<Self::Held, Error>;
    }

    impl<T: ?Sized> ViaRefNone for RefProbe<T> {
        type Held = Unconstructible<T>;
        fn scry_ref_arg(&self, _value: &Value) -> Result<Unconstructible<T>, Error> {
            Err(Error::CannotConvert { type_name: type_name::<&T>() })
        }
    }
}

/// What [`lend::ViaRefNone`] would lend, if it ever produced a value; it never does.
pub struct Unconstructible<T: ?Sized>(Infallible, PhantomData<fn() -> Box<T>>);

impl<T: ?Sized> Borrow<T> for Unconstructible<T> {
    fn borrow(&self) -> &T {
        match self.0 {}
    }
}
