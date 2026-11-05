//! Peeling: reaching a value of a foreign type that sits inside a generic container.
//!
//! The probes pick how to handle a value from what its type implements, and Rust only sees the
//! outer type: `Mutex<Tag>` is `Reflect` when `Tag` is, and when it is not, no impl looks inside.
//! So a `Tag` that only has `FromStr` and `Debug` would be shown as the whole lock's `Debug` text,
//! and could not be written.
//!
//! The derive and the generator see the type as written, though. For a field of type
//! `Arc<Mutex<Vec<Tag>>>` they pass the probe a peeler built from that spelling -
//! `Shared(Locked(Each(leaf::<Tag>(..))))` - whose leaf probes `Tag` where it is concrete. The
//! peeler is a tier of its own, below `Reflect` and serde, so it only decides for values nothing
//! better handles, and a peeler that does not fit the real type (a `Mutex` that is not std's or
//! `lock_api`'s) simply never applies. Each layer:
//!
//! - [`Locked`]: a lock or cell (a [`Wrapper`]), read and written under its own locking.
//! - [`Shared`]: `Arc`, `Rc` and `LazyLock`, seen through; writable only through interior mutability.
//! - [`Boxed`]: `Box`, seen through.
//! - [`Opt`]: `Option` and `OnceLock`; assigning into an `Option` that is `None` builds the value.
//! - [`Each`]: `Vec`, `VecDeque`, arrays, `HashMap` and `BTreeMap` (by the keys' `Display`); each
//!   element is reached and written in place, but the collection is not replaced as a whole.
//!
//! A type alias hides the spelling, so a field declared through one is not peeled.

use std::any::type_name;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Display;
use std::hash::{BuildHasher, Hash};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, OnceLock};

use serde_json::Value;

use crate::__private::{FieldMut, FieldRef, truncated};
use crate::probes::Wrapper;
use crate::{Error, Kind, Member, Reflect, Segment, Visit, VisitMut};

/// The most elements a peeled collection lists.
const MAX_ELEMENTS: usize = 100;

/// How to take apart a value of type `T` to reach what it holds.
pub trait Peeler<T>: Copy + 'static {
    fn peel_ref<'b>(&self, value: &'b T) -> FieldRef<'b>;
    fn peel_mut<'b>(&self, value: &'b mut T) -> FieldMut<'b>;

    /// A new `T` from JSON, for assigning into `None`.
    fn build(&self, value: &Value) -> Result<T, Error> {
        let _ = value;
        Err(Error::CannotConvert { type_name: type_name::<T>() })
    }
}

/// The peeler of a probe that has none: implements no [`Peeler`], so that tier never applies.
#[derive(Clone, Copy)]
pub struct NoPeel;

// --- Leaf ---------------------------------------------------------------------------------------

/// The innermost type, handled by the probes at the place it was named.
pub struct Leaf<X> {
    read: for<'b> fn(&'b X) -> FieldRef<'b>,
    write: for<'b> fn(&'b mut X) -> FieldMut<'b>,
    build: fn(&Value) -> Result<X, Error>,
}

impl<X> Clone for Leaf<X> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<X> Copy for Leaf<X> {}

/// A [`Leaf`]; generated code passes closures that run the probe macros on `X`.
pub fn leaf<X>(
    read: for<'b> fn(&'b X) -> FieldRef<'b>,
    write: for<'b> fn(&'b mut X) -> FieldMut<'b>,
    build: fn(&Value) -> Result<X, Error>,
) -> Leaf<X> {
    Leaf { read, write, build }
}

impl<X: 'static> Peeler<X> for Leaf<X> {
    fn peel_ref<'b>(&self, value: &'b X) -> FieldRef<'b> {
        (self.read)(value)
    }

    fn peel_mut<'b>(&self, value: &'b mut X) -> FieldMut<'b> {
        (self.write)(value)
    }

    fn build(&self, value: &Value) -> Result<X, Error> {
        (self.build)(value)
    }
}

// --- Locks and cells ----------------------------------------------------------------------------

/// A lock or cell: its value is peeled under the lock.
#[derive(Clone, Copy)]
pub struct Locked<P>(pub P);

impl<W: Wrapper, P: Peeler<W::Inner>> Peeler<W> for Locked<P> {
    fn peel_ref<'b>(&self, value: &'b W) -> FieldRef<'b> {
        FieldRef::Owned(Box::new(LockedView { wrapper: value, peel: self.0 }))
    }

    fn peel_mut<'b>(&self, value: &'b mut W) -> FieldMut<'b> {
        FieldMut::Owned(Box::new(LockedView { wrapper: &*value, peel: self.0 }))
    }
}

struct LockedView<'b, W, P> {
    wrapper: &'b W,
    peel: P,
}

impl<W: Wrapper, P: Peeler<W::Inner>> LockedView<'_, W, P> {
    fn inside<R>(&self, f: impl FnOnce(FieldRef<'_>) -> R) -> Result<R, Error> {
        let (mut f, mut out) = (Some(f), None);
        self.wrapper.read_inner(&mut |inner| out = f.take().map(|f| f(self.peel.peel_ref(inner))))?;
        out.ok_or(Error::LockBusy { type_name: type_name::<W>() })
    }

    fn inside_mut<R>(&self, f: impl FnOnce(FieldMut<'_>) -> R) -> Result<R, Error> {
        let (mut f, mut out) = (Some(f), None);
        self.wrapper.write_inner(&mut |inner| out = f.take().map(|f| f(self.peel.peel_mut(inner))))?;
        out.ok_or(Error::LockBusy { type_name: type_name::<W>() })
    }
}

impl<W: Wrapper, P: Peeler<W::Inner>> Reflect for LockedView<'_, W, P> {
    fn type_name(&self) -> &'static str {
        type_name::<W>()
    }

    fn kind(&self) -> Kind {
        self.inside(|value| value.get().kind()).unwrap_or(Kind::Opaque)
    }

    fn members(&self) -> Vec<Member> {
        self.inside(|value| value.get().members()).unwrap_or_default()
    }

    fn member_count(&self) -> usize {
        self.inside(|value| value.get().member_count()).unwrap_or(0)
    }

    fn snapshot(&self, depth: usize) -> Value {
        self.inside(|value| value.snapshot(depth)).unwrap_or_else(|_| {
            Value::String(format!("<locked: {}>", crate::reflect::short_type_name(type_name::<W>())))
        })
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        self.inside(|value| value.read(path, visit))?
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.inside_mut(|value| value.write(path, visit))?
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        self.inside_mut(|target| target.assign(value))?
    }
}

// --- Pointers -----------------------------------------------------------------------------------

/// `Arc` or `Rc`: seen through; what it holds is only written through interior mutability.
#[derive(Clone, Copy)]
pub struct Shared<P>(pub P);

macro_rules! shared_peeler {
    ($pointer:ident) => {
        impl<X, P: Peeler<X>> Peeler<$pointer<X>> for Shared<P> {
            fn peel_ref<'b>(&self, value: &'b $pointer<X>) -> FieldRef<'b> {
                self.0.peel_ref(&**value)
            }

            fn peel_mut<'b>(&self, value: &'b mut $pointer<X>) -> FieldMut<'b> {
                FieldMut::Owned(Box::new(SharedOnly(self.0.peel_ref(&**value))))
            }

            fn build(&self, value: &Value) -> Result<$pointer<X>, Error> {
                self.0.build(value).map($pointer::new)
            }
        }
    };
}

shared_peeler!(Arc);
shared_peeler!(Rc);

/// Like an `Arc`; reading one that has not run yet runs it, as its `Reflect` impl does.
impl<X, F: FnOnce() -> X, P: Peeler<X>> Peeler<LazyLock<X, F>> for Shared<P> {
    fn peel_ref<'b>(&self, value: &'b LazyLock<X, F>) -> FieldRef<'b> {
        self.0.peel_ref(LazyLock::force(value))
    }

    fn peel_mut<'b>(&self, value: &'b mut LazyLock<X, F>) -> FieldMut<'b> {
        FieldMut::Owned(Box::new(SharedOnly(self.0.peel_ref(LazyLock::force(value)))))
    }
}

/// A value reached through shared access, offered for writing: writes go to its interior
/// mutability, if it has any.
struct SharedOnly<'b>(FieldRef<'b>);

impl Reflect for SharedOnly<'_> {
    fn type_name(&self) -> &'static str {
        self.0.get().type_name()
    }

    fn kind(&self) -> Kind {
        self.0.get().kind()
    }

    fn members(&self) -> Vec<Member> {
        self.0.get().members()
    }

    fn member_count(&self) -> usize {
        self.0.get().member_count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        self.0.snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        self.0.read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.0.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.0.write_shared(path, visit)
    }

    fn assign(&mut self, _value: &Value) -> Result<(), Error> {
        Err(Error::needs_interior_mutability(self.type_name()))
    }
}

/// `Box`: seen through.
#[derive(Clone, Copy)]
pub struct Boxed<P>(pub P);

impl<X, P: Peeler<X>> Peeler<Box<X>> for Boxed<P> {
    fn peel_ref<'b>(&self, value: &'b Box<X>) -> FieldRef<'b> {
        self.0.peel_ref(&**value)
    }

    fn peel_mut<'b>(&self, value: &'b mut Box<X>) -> FieldMut<'b> {
        self.0.peel_mut(&mut **value)
    }

    fn build(&self, value: &Value) -> Result<Box<X>, Error> {
        self.0.build(value).map(Box::new)
    }
}

// --- Option -------------------------------------------------------------------------------------

/// `Option`: `null` when `None`; assigning `null` clears it, and assigning into `None` builds the
/// value.
#[derive(Clone, Copy)]
pub struct Opt<P>(pub P);

impl<X: 'static, P: Peeler<X>> Peeler<Option<X>> for Opt<P> {
    fn peel_ref<'b>(&self, value: &'b Option<X>) -> FieldRef<'b> {
        match value {
            Some(value) => self.0.peel_ref(value),
            None => FieldRef::Owned(Box::new(Value::Null)),
        }
    }

    fn peel_mut<'b>(&self, value: &'b mut Option<X>) -> FieldMut<'b> {
        FieldMut::Owned(Box::new(OptMut { value, peel: self.0 }))
    }

    fn build(&self, value: &Value) -> Result<Option<X>, Error> {
        if value.is_null() { Ok(None) } else { self.0.build(value).map(Some) }
    }
}

/// A `OnceLock`: like an `Option` that is only ever set once, so it is read, not assigned.
impl<X: 'static, P: Peeler<X>> Peeler<OnceLock<X>> for Opt<P> {
    fn peel_ref<'b>(&self, value: &'b OnceLock<X>) -> FieldRef<'b> {
        match value.get() {
            Some(value) => self.0.peel_ref(value),
            None => FieldRef::Owned(Box::new(Value::Null)),
        }
    }

    fn peel_mut<'b>(&self, value: &'b mut OnceLock<X>) -> FieldMut<'b> {
        match value.get_mut() {
            Some(value) => self.0.peel_mut(value),
            None => FieldMut::Owned(Box::new(SharedOnly(FieldRef::Owned(Box::new(Value::Null))))),
        }
    }
}

struct OptMut<'b, X, P> {
    value: &'b mut Option<X>,
    peel: P,
}

impl<X: 'static, P: Peeler<X>> Reflect for OptMut<'_, X, P> {
    fn type_name(&self) -> &'static str {
        type_name::<Option<X>>()
    }

    fn kind(&self) -> Kind {
        Opt(self.peel).peel_ref(self.value).get().kind()
    }

    fn members(&self) -> Vec<Member> {
        Opt(self.peel).peel_ref(self.value).get().members()
    }

    fn snapshot(&self, depth: usize) -> Value {
        Opt(self.peel).peel_ref(self.value).snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { Opt(self.peel).peel_ref(self.value).read(path, visit) }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let Some(segment) = path.first() else { return visit(self) };
        match self.value {
            Some(value) => self.peel.peel_mut(value).write(path, visit),
            None => Err(crate::__private::no_member(type_name::<Option<X>>(), segment)),
        }
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        Opt(self.peel).peel_ref(self.value).write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        if value.is_null() {
            *self.value = None;
        } else if let Some(inner) = self.value {
            self.peel.peel_mut(inner).assign(value)?;
        } else {
            *self.value = Some(self.peel.build(value)?);
        }
        Ok(())
    }
}

// --- Collections --------------------------------------------------------------------------------

/// A collection's elements: by position, or by the `Display` form of their keys.
pub trait Elements {
    type Item;
    const KEYED: bool;
    fn count(&self) -> usize;
    fn each(&self) -> Box<dyn Iterator<Item = (String, &Self::Item)> + '_>;
    fn find(&self, segment: &Segment) -> Option<&Self::Item>;
    fn find_mut(&mut self, segment: &Segment) -> Option<&mut Self::Item>;
}

macro_rules! sequence_elements {
    ([$($generics:tt)*] $collection:ty) => {
        impl<$($generics)*> Elements for $collection {
            type Item = X;
            const KEYED: bool = false;

            fn count(&self) -> usize {
                self.len()
            }

            fn each(&self) -> Box<dyn Iterator<Item = (String, &X)> + '_> {
                Box::new(self.iter().enumerate().map(|(index, item)| (format!("[{index}]"), item)))
            }

            fn find(&self, segment: &Segment) -> Option<&X> {
                segment.index().and_then(|index| self.get(index))
            }

            fn find_mut(&mut self, segment: &Segment) -> Option<&mut X> {
                segment.index().and_then(|index| self.get_mut(index))
            }
        }
    };
}

sequence_elements!([X] Vec<X>);
sequence_elements!([X] VecDeque<X>);
sequence_elements!([X, const N: usize] [X; N]);

macro_rules! map_elements {
    ([$($generics:tt)*] $collection:ty) => {
        impl<$($generics)*> Elements for $collection {
            type Item = X;
            const KEYED: bool = true;

            fn count(&self) -> usize {
                self.len()
            }

            fn each(&self) -> Box<dyn Iterator<Item = (String, &X)> + '_> {
                Box::new(self.iter().map(|(key, item)| (key.to_string(), item)))
            }

            fn find(&self, segment: &Segment) -> Option<&X> {
                let name = segment.name();
                self.iter().find(|(key, _)| key.to_string() == *name).map(|(_, item)| item)
            }

            fn find_mut(&mut self, segment: &Segment) -> Option<&mut X> {
                let name = segment.name();
                self.iter_mut().find(|(key, _)| key.to_string() == *name).map(|(_, item)| item)
            }
        }
    };
}

map_elements!([K: Display + Eq + Hash, X, S: BuildHasher] HashMap<K, X, S>);
map_elements!([K: Display + Ord, X] BTreeMap<K, X>);

/// A collection: each element is peeled.
#[derive(Clone, Copy)]
pub struct Each<P>(pub P);

impl<C: Elements, P: Peeler<C::Item>> Peeler<C> for Each<P> {
    fn peel_ref<'b>(&self, value: &'b C) -> FieldRef<'b> {
        FieldRef::Owned(Box::new(EachView { items: value, peel: self.0 }))
    }

    fn peel_mut<'b>(&self, value: &'b mut C) -> FieldMut<'b> {
        FieldMut::Owned(Box::new(EachMut { items: value, peel: self.0 }))
    }
}

struct EachView<'b, C, P> {
    items: &'b C,
    peel: P,
}

impl<C: Elements, P: Peeler<C::Item>> Reflect for EachView<'_, C, P> {
    fn type_name(&self) -> &'static str {
        type_name::<C>()
    }

    fn kind(&self) -> Kind {
        if C::KEYED { Kind::Struct } else { Kind::Sequence }
    }

    fn members(&self) -> Vec<Member> {
        self.items
            .each()
            .take(MAX_ELEMENTS)
            .map(|(name, item)| Member::new(name, self.peel.peel_ref(item).get().type_name()))
            .collect()
    }

    fn member_count(&self) -> usize {
        self.items.count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        if depth == 0 {
            return truncated(type_name::<C>());
        }
        let items = self.items.each().take(MAX_ELEMENTS);
        if C::KEYED {
            Value::Object(items.map(|(name, item)| (name, self.peel.peel_ref(item).snapshot(depth - 1))).collect())
        } else {
            Value::Array(items.map(|(_, item)| self.peel.peel_ref(item).snapshot(depth - 1)).collect())
        }
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        match path.split_first() {
            None => visit(self),
            Some((segment, rest)) => match self.items.find(segment) {
                Some(item) => self.peel.peel_ref(item).read(rest, visit),
                None => Err(crate::__private::no_member(type_name::<C>(), segment)),
            },
        }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        match path.split_first() {
            None => Err(Error::needs_interior_mutability(type_name::<C>())),
            Some((segment, rest)) => match self.items.find(segment) {
                Some(item) => self.peel.peel_ref(item).write_shared(rest, visit),
                None => Err(crate::__private::no_member(type_name::<C>(), segment)),
            },
        }
    }
}

struct EachMut<'b, C, P> {
    items: &'b mut C,
    peel: P,
}

impl<C: Elements, P: Peeler<C::Item>> EachMut<'_, C, P> {
    fn view(&self) -> EachView<'_, C, P> {
        EachView { items: &*self.items, peel: self.peel }
    }
}

impl<C: Elements, P: Peeler<C::Item>> Reflect for EachMut<'_, C, P> {
    fn type_name(&self) -> &'static str {
        type_name::<C>()
    }

    fn kind(&self) -> Kind {
        self.view().kind()
    }

    fn members(&self) -> Vec<Member> {
        self.view().members()
    }

    fn member_count(&self) -> usize {
        self.view().member_count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        self.view().snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        match path {
            [] => visit(self),
            _ => self.view().read(path, visit),
        }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        match path.split_first() {
            None => visit(self),
            Some((segment, rest)) => match self.items.find_mut(segment) {
                Some(item) => self.peel.peel_mut(item).write(rest, visit),
                None => Err(crate::__private::no_member(type_name::<C>(), segment)),
            },
        }
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.view().write_shared(path, visit)
    }

    fn assign(&mut self, _value: &Value) -> Result<(), Error> {
        Err(Error::not_writable(format!(
            "{} is only written element by element",
            crate::reflect::short_type_name(type_name::<C>())
        )))
    }
}
