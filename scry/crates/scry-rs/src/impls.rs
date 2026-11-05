//! `Reflect` for std types.

use std::any::type_name;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Display;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicI64, AtomicIsize, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock, TryLockError, TryLockResult};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::__private::truncated;
use crate::{Error, Kind, Member, Reflect, Segment, Visit, VisitMut};

/// Snapshots list at most this many elements of a sequence or map.
const MAX_ELEMENTS: usize = 100;

/// How long a read or write waits for a lock the application is holding before giving up with
/// `lock_busy`. A try-lock loop rather than a blocking lock, so a registry driven from a thread
/// that already holds the lock reports an error instead of deadlocking.
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

macro_rules! leaf_access {
    () => {
        fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
            if path.is_empty() { visit(self) } else { Err(Error::NotNavigable { type_name: self.type_name() }) }
        }

        fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
            if path.is_empty() { visit(self) } else { Err(Error::NotNavigable { type_name: self.type_name() }) }
        }
    };
}

// --- Scalars ------------------------------------------------------------------------------------

macro_rules! integer_impls {
    ($($t:ty),*) => {$(
        impl Reflect for $t {
            fn type_name(&self) -> &'static str { stringify!($t) }
            fn kind(&self) -> Kind { Kind::Scalar }
            fn snapshot(&self, _depth: usize) -> Value { Value::from(*self) }
            leaf_access!();

            fn assign(&mut self, value: &Value) -> Result<(), Error> {
                let parsed = value
                    .as_i64()
                    .and_then(|v| <$t>::try_from(v).ok())
                    .or_else(|| value.as_u64().and_then(|v| <$t>::try_from(v).ok()));
                *self = parsed.ok_or_else(|| Error::mismatch(concat!("an integer that fits ", stringify!($t)), value))?;
                Ok(())
            }
        }
    )*};
}

integer_impls!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

/// 128-bit integers do not fit a JSON number, so they travel as strings.
macro_rules! wide_integer_impls {
    ($($t:ty),*) => {$(
        impl Reflect for $t {
            fn type_name(&self) -> &'static str { stringify!($t) }
            fn kind(&self) -> Kind { Kind::Scalar }
            fn snapshot(&self, _depth: usize) -> Value { Value::String(self.to_string()) }
            leaf_access!();

            fn assign(&mut self, value: &Value) -> Result<(), Error> {
                let text = match value {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    _ => return Err(Error::mismatch(stringify!($t), value)),
                };
                *self = text.parse().map_err(|_| Error::mismatch(stringify!($t), value))?;
                Ok(())
            }
        }
    )*};
}

wide_integer_impls!(i128, u128);

macro_rules! float_impls {
    ($($t:ty),*) => {$(
        impl Reflect for $t {
            fn type_name(&self) -> &'static str { stringify!($t) }
            fn kind(&self) -> Kind { Kind::Scalar }
            fn snapshot(&self, _depth: usize) -> Value { Value::from(f64::from(*self)) }
            leaf_access!();

            fn assign(&mut self, value: &Value) -> Result<(), Error> {
                let number = value.as_f64().ok_or_else(|| Error::mismatch("a number", value))?;
                *self = number as $t;
                Ok(())
            }
        }
    )*};
}

float_impls!(f32, f64);

impl Reflect for bool {
    fn type_name(&self) -> &'static str {
        "bool"
    }

    fn kind(&self) -> Kind {
        Kind::Scalar
    }

    fn snapshot(&self, _depth: usize) -> Value {
        Value::Bool(*self)
    }

    leaf_access!();

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        *self = value.as_bool().ok_or_else(|| Error::mismatch("true or false", value))?;
        Ok(())
    }
}

impl Reflect for char {
    fn type_name(&self) -> &'static str {
        "char"
    }

    fn kind(&self) -> Kind {
        Kind::Scalar
    }

    fn snapshot(&self, _depth: usize) -> Value {
        Value::String(self.to_string())
    }

    leaf_access!();

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        let mut chars = value.as_str().map(str::chars).ok_or_else(|| Error::mismatch("a one-character string", value))?;
        match (chars.next(), chars.next()) {
            (Some(c), None) => {
                *self = c;
                Ok(())
            }
            _ => Err(Error::mismatch("a one-character string", value)),
        }
    }
}

impl Reflect for String {
    fn type_name(&self) -> &'static str {
        "String"
    }

    fn kind(&self) -> Kind {
        Kind::Scalar
    }

    fn snapshot(&self, _depth: usize) -> Value {
        Value::String(self.clone())
    }

    leaf_access!();

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        value.as_str().ok_or_else(|| Error::mismatch("a string", value))?.clone_into(self);
        Ok(())
    }
}

/// Readable only: a `&str` field cannot be pointed at a string created at runtime.
impl Reflect for &str {
    fn type_name(&self) -> &'static str {
        "&str"
    }

    fn kind(&self) -> Kind {
        Kind::Scalar
    }

    fn snapshot(&self, _depth: usize) -> Value {
        Value::String((*self).to_owned())
    }

    leaf_access!();
}

impl Reflect for () {
    fn type_name(&self) -> &'static str {
        "()"
    }

    fn kind(&self) -> Kind {
        Kind::Scalar
    }

    fn snapshot(&self, _depth: usize) -> Value {
        Value::Null
    }

    leaf_access!();
}

/// Seconds, as a fractional number.
impl Reflect for Duration {
    fn type_name(&self) -> &'static str {
        "Duration"
    }

    fn kind(&self) -> Kind {
        Kind::Scalar
    }

    fn snapshot(&self, _depth: usize) -> Value {
        Value::from(self.as_secs_f64())
    }

    leaf_access!();

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        let seconds = value.as_f64().ok_or_else(|| Error::mismatch("a number of seconds", value))?;
        *self = Duration::try_from_secs_f64(seconds).map_err(|_| Error::mismatch("a non-negative number of seconds", value))?;
        Ok(())
    }
}

/// Readable as its age in seconds, which says more than the opaque counter it wraps.
impl Reflect for Instant {
    fn type_name(&self) -> &'static str {
        "Instant"
    }

    fn kind(&self) -> Kind {
        Kind::Scalar
    }

    fn snapshot(&self, _depth: usize) -> Value {
        let mut object = Map::new();
        object.insert("elapsed_secs".into(), Value::from(self.elapsed().as_secs_f64()));
        Value::Object(object)
    }

    leaf_access!();
}

// --- Option ------------------------------------------------------------------------------------

/// Transparent when `Some`; an empty path still resolves to the option itself, so `None` reads as
/// `null` and assigning `null` clears it.
impl<T: Reflect> Reflect for Option<T> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        self.as_ref().map_or(Kind::Scalar, Reflect::kind)
    }

    fn members(&self) -> Vec<Member> {
        self.as_ref().map(Reflect::members).unwrap_or_default()
    }

    fn member_count(&self) -> usize {
        self.as_ref().map_or(0, Reflect::member_count)
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.as_ref().and_then(Reflect::as_any)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        self.as_mut().and_then(Reflect::as_any_mut)
    }

    fn snapshot(&self, depth: usize) -> Value {
        self.as_ref().map_or(Value::Null, |value| value.snapshot(depth))
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        match self {
            _ if path.is_empty() => visit(self),
            Some(value) => value.read(path, visit),
            None => Err(Error::Empty { type_name: self.type_name() }),
        }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        if path.is_empty() {
            return visit(self);
        }
        let type_name = self.type_name();
        self.as_mut().ok_or(Error::Empty { type_name })?.write(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.as_ref().ok_or(Error::Empty { type_name: self.type_name() })?.write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        match self {
            _ if value.is_null() => {
                *self = None;
                Ok(())
            }
            Some(inner) => inner.assign(value),
            None => Err(Error::not_writable(format!(
                "{} is None, and a value cannot be created inside it from JSON",
                type_name::<Self>()
            ))),
        }
    }
}

// --- Sequences ---------------------------------------------------------------------------------

fn sequence_members<T>(items: &[T]) -> Vec<Member> {
    (0..items.len().min(MAX_ELEMENTS)).map(|index| Member::new(format!("[{index}]"), type_name::<T>())).collect()
}

fn sequence_snapshot<T: Reflect>(items: &[T], type_name: &'static str, depth: usize) -> Value {
    if depth == 0 {
        return truncated(type_name);
    }
    let mut values: Vec<Value> = items.iter().take(MAX_ELEMENTS).map(|item| item.snapshot(depth - 1)).collect();
    if items.len() > MAX_ELEMENTS {
        values.push(Value::String(format!("… {} more", items.len() - MAX_ELEMENTS)));
    }
    Value::Array(values)
}

fn sequence_index(segment: &Segment, len: usize) -> Result<usize, Error> {
    let index = segment.index().ok_or_else(|| Error::BadPath(format!("'{segment}' is not an index")))?;
    if index < len { Ok(index) } else { Err(Error::IndexOutOfRange { len, index }) }
}

fn sequence_read<T: Reflect>(items: &[T], path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
    let (segment, rest) = path.split_first().expect("callers handle the empty path");
    items[sequence_index(segment, items.len())?].read(rest, visit)
}

fn sequence_write<T: Reflect>(items: &mut [T], path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
    let (segment, rest) = path.split_first().expect("callers handle the empty path");
    let index = sequence_index(segment, items.len())?;
    items[index].write(rest, visit)
}

fn sequence_write_shared<T: Reflect>(
    items: &[T],
    type_name: &'static str,
    path: &[Segment],
    visit: VisitMut<'_>,
) -> Result<(), Error> {
    let Some((segment, rest)) = path.split_first() else {
        return Err(Error::needs_interior_mutability(type_name));
    };
    items[sequence_index(segment, items.len())?].write_shared(rest, visit)
}

/// Element-wise, and only for an array of the same length: a sequence cannot grow or shrink from
/// JSON, because that would need to construct new elements.
fn sequence_assign<T: Reflect>(items: &mut [T], value: &Value) -> Result<(), Error> {
    let values = value.as_array().ok_or_else(|| Error::mismatch("an array", value))?;
    if values.len() != items.len() {
        return Err(Error::mismatch("an array of the same length", value));
    }
    items.iter_mut().zip(values).try_for_each(|(item, value)| item.assign(value))
}

/// Shown as `{"Ok": value}` or `{"Err": error}`, the way the derive shows an enum, and navigated
/// through `.Ok` and `.Err`.
impl<T: Reflect, E: Reflect> Reflect for Result<T, E> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        Kind::Enum
    }

    fn members(&self) -> Vec<Member> {
        match self {
            Ok(_) => vec![Member::new("Ok", type_name::<T>())],
            Err(_) => vec![Member::new("Err", type_name::<E>())],
        }
    }

    fn snapshot(&self, depth: usize) -> Value {
        if depth == 0 {
            return truncated(self.type_name());
        }
        let (variant, value) = match self {
            Ok(value) => ("Ok", value.snapshot(depth - 1)),
            Err(error) => ("Err", error.snapshot(depth - 1)),
        };
        Value::Object(Map::from_iter([(variant.to_owned(), value)]))
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else { return visit(self) };
        match (self, &*segment.name()) {
            (Ok(value), "Ok") => value.read(rest, visit),
            (Err(error), "Err") => error.read(rest, visit),
            _ => Err(crate::__private::no_member(self.type_name(), segment)),
        }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else { return visit(self) };
        let type_name = self.type_name();
        match (self, &*segment.name()) {
            (Ok(value), "Ok") => value.write(rest, visit),
            (Err(error), "Err") => error.write(rest, visit),
            _ => Err(crate::__private::no_member(type_name, segment)),
        }
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else {
            return Err(Error::needs_interior_mutability(self.type_name()));
        };
        match (self, &*segment.name()) {
            (Ok(value), "Ok") => value.write_shared(rest, visit),
            (Err(error), "Err") => error.write_shared(rest, visit),
            _ => Err(crate::__private::no_member(self.type_name(), segment)),
        }
    }
}

impl<T: Reflect> Reflect for Vec<T> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        Kind::Sequence
    }

    fn members(&self) -> Vec<Member> {
        sequence_members(self)
    }

    fn member_count(&self) -> usize {
        <[_]>::len(self)
    }

    fn snapshot(&self, depth: usize) -> Value {
        sequence_snapshot(self, self.type_name(), depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { sequence_read(self, path, visit) }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { sequence_write(self, path, visit) }
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        sequence_write_shared(self, self.type_name(), path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        sequence_assign(self, value)
    }
}

/// A borrowed slice, as functions return them. Its elements cannot be written through it.
impl<T: Reflect> Reflect for &[T] {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        Kind::Sequence
    }

    fn members(&self) -> Vec<Member> {
        sequence_members(self)
    }

    fn member_count(&self) -> usize {
        <[_]>::len(self)
    }

    fn snapshot(&self, depth: usize) -> Value {
        sequence_snapshot(self, self.type_name(), depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { sequence_read(self, path, visit) }
    }

    fn write(&mut self, _path: &[Segment], _visit: VisitMut<'_>) -> Result<(), Error> {
        Err(Error::not_writable(format!("{} is a shared borrow", self.type_name())))
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        sequence_write_shared(self, self.type_name(), path, visit)
    }
}

impl<T: Reflect, const N: usize> Reflect for [T; N] {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        Kind::Sequence
    }

    fn members(&self) -> Vec<Member> {
        sequence_members(self)
    }

    fn member_count(&self) -> usize {
        <[_]>::len(self)
    }

    fn snapshot(&self, depth: usize) -> Value {
        sequence_snapshot(self, self.type_name(), depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { sequence_read(self, path, visit) }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        if path.is_empty() { visit(self) } else { sequence_write(self, path, visit) }
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        sequence_write_shared(self, self.type_name(), path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        sequence_assign(self, value)
    }
}

// --- Tuples ------------------------------------------------------------------------------------

macro_rules! tuple_impls {
    ($(($($name:ident $index:tt),+))*) => {$(
        impl<$($name: Reflect),+> Reflect for ($($name,)+) {
            fn type_name(&self) -> &'static str { type_name::<Self>() }
            fn kind(&self) -> Kind { Kind::Tuple }

            fn members(&self) -> Vec<Member> {
                vec![$(Member::new(stringify!($index), type_name::<$name>())),+]
            }

            fn snapshot(&self, depth: usize) -> Value {
                if depth == 0 {
                    return truncated(self.type_name());
                }
                Value::Array(vec![$(self.$index.snapshot(depth - 1)),+])
            }

            fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
                let Some((segment, rest)) = path.split_first() else { return visit(self) };
                match segment.index() {
                    $(Some($index) => self.$index.read(rest, visit),)+
                    _ => Err(crate::__private::no_member(self.type_name(), segment)),
                }
            }

            fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
                let Some((segment, rest)) = path.split_first() else { return visit(self) };
                match segment.index() {
                    $(Some($index) => self.$index.write(rest, visit),)+
                    _ => Err(crate::__private::no_member(self.type_name(), segment)),
                }
            }

            fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
                let Some((segment, rest)) = path.split_first() else {
                    return Err(Error::needs_interior_mutability(self.type_name()));
                };
                match segment.index() {
                    $(Some($index) => self.$index.write_shared(rest, visit),)+
                    _ => Err(crate::__private::no_member(self.type_name(), segment)),
                }
            }
        }
    )*};
}

tuple_impls! {
    (A 0)
    (A 0, B 1)
    (A 0, B 1, C 2)
    (A 0, B 1, C 2, D 3)
}

// --- Maps --------------------------------------------------------------------------------------

/// Maps are navigated by the `Display` form of their keys (`.hammer`, `.42`), found by a linear
/// scan, so any key type with a readable rendering works without parsing keys back.
macro_rules! map_impl {
    ($map:ident, $($bound:path),*) => {
        impl<K: Display $(+ $bound)*, V: Reflect> Reflect for $map<K, V> {
            fn type_name(&self) -> &'static str { type_name::<Self>() }
            fn kind(&self) -> Kind { Kind::Struct }

            fn members(&self) -> Vec<Member> {
                self.keys().take(MAX_ELEMENTS).map(|key| Member::new(key.to_string(), type_name::<V>())).collect()
            }

            fn member_count(&self) -> usize {
                $map::len(self)
            }

            fn snapshot(&self, depth: usize) -> Value {
                if depth == 0 {
                    return truncated(self.type_name());
                }
                let mut object = Map::new();
                for (key, value) in self.iter().take(MAX_ELEMENTS) {
                    object.insert(key.to_string(), value.snapshot(depth - 1));
                }
                Value::Object(object)
            }

            fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
                let Some((segment, rest)) = path.split_first() else { return visit(self) };
                let wanted = segment.to_string();
                match self.iter().find(|(key, _)| key.to_string() == wanted) {
                    Some((_, value)) => value.read(rest, visit),
                    None => Err(crate::__private::no_member(self.type_name(), segment)),
                }
            }

            fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
                let Some((segment, rest)) = path.split_first() else { return visit(self) };
                let wanted = segment.to_string();
                let type_name = self.type_name();
                match self.iter_mut().find(|(key, _)| key.to_string() == wanted) {
                    Some((_, value)) => value.write(rest, visit),
                    None => Err(crate::__private::no_member(type_name, segment)),
                }
            }

            fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
                let Some((segment, rest)) = path.split_first() else {
                    return Err(Error::needs_interior_mutability(self.type_name()));
                };
                let wanted = segment.to_string();
                match self.iter().find(|(key, _)| key.to_string() == wanted) {
                    Some((_, value)) => value.write_shared(rest, visit),
                    None => Err(crate::__private::no_member(self.type_name(), segment)),
                }
            }
        }
    };
}

map_impl!(BTreeMap, Ord);
map_impl!(HashMap, Eq, std::hash::Hash);

// --- Transparent wrappers ----------------------------------------------------------------------

/// Fully transparent. Reports the inner value's type name, which for a `Box<dyn Trait>` is the
/// concrete type behind it - the one place a trait object gives its real type away.
impl<T: Reflect + ?Sized> Reflect for Box<T> {
    fn type_name(&self) -> &'static str {
        (**self).type_name()
    }

    fn kind(&self) -> Kind {
        (**self).kind()
    }

    fn members(&self) -> Vec<Member> {
        (**self).members()
    }

    fn member_count(&self) -> usize {
        (**self).member_count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        (**self).snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        (**self).read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        (**self).write(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        (**self).write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        (**self).assign(value)
    }
}

/// Transparent for reading. An `Arc` only ever gives shared access to its contents, so even an
/// exclusive path through it continues as a shared one and needs interior mutability below.
impl<T: Reflect + ?Sized> Reflect for Arc<T> {
    fn type_name(&self) -> &'static str {
        (**self).type_name()
    }

    fn kind(&self) -> Kind {
        (**self).kind()
    }

    fn members(&self) -> Vec<Member> {
        (**self).members()
    }

    fn member_count(&self) -> usize {
        (**self).member_count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        (**self).snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        (**self).read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        (**self).write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        (**self).write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        (**self).write_shared(&[], &mut |inner| inner.assign(value))
    }
}

/// Transparent, shared like an `Arc`: a write continues below only through interior mutability.
impl<T: Reflect + ?Sized> Reflect for &T {
    fn type_name(&self) -> &'static str {
        (**self).type_name()
    }

    fn kind(&self) -> Kind {
        (**self).kind()
    }

    fn members(&self) -> Vec<Member> {
        (**self).members()
    }

    fn member_count(&self) -> usize {
        (**self).member_count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        (**self).snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        (**self).read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        (**self).write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        (**self).write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        (**self).write_shared(&[], &mut |inner| inner.assign(value))
    }
}

/// Fully transparent, like a `Box`.
impl<T: Reflect + ?Sized> Reflect for &mut T {
    fn type_name(&self) -> &'static str {
        (**self).type_name()
    }

    fn kind(&self) -> Kind {
        (**self).kind()
    }

    fn members(&self) -> Vec<Member> {
        (**self).members()
    }

    fn member_count(&self) -> usize {
        (**self).member_count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        (**self).snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        (**self).read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        (**self).write(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        (**self).write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        (**self).assign(value)
    }
}

/// Transparent, shared like an `Arc`. Reading a `LazyLock` that has not run yet runs it, exactly
/// as the app's own first access would.
impl<T: Reflect, F: FnOnce() -> T> Reflect for LazyLock<T, F> {
    fn type_name(&self) -> &'static str {
        LazyLock::force(self).type_name()
    }

    fn kind(&self) -> Kind {
        LazyLock::force(self).kind()
    }

    fn members(&self) -> Vec<Member> {
        LazyLock::force(self).members()
    }

    fn member_count(&self) -> usize {
        LazyLock::force(self).member_count()
    }

    fn snapshot(&self, depth: usize) -> Value {
        LazyLock::force(self).snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        LazyLock::force(self).read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        LazyLock::force(self).write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        LazyLock::force(self).write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        LazyLock::force(self).write_shared(&[], &mut |inner| inner.assign(value))
    }
}

impl<T: Reflect> Reflect for OnceLock<T> {
    fn type_name(&self) -> &'static str {
        self.get().map_or_else(type_name::<Self>, Reflect::type_name)
    }

    fn kind(&self) -> Kind {
        self.get().map_or(Kind::Opaque, Reflect::kind)
    }

    fn members(&self) -> Vec<Member> {
        self.get().map(Reflect::members).unwrap_or_default()
    }

    fn member_count(&self) -> usize {
        self.get().map_or(0, Reflect::member_count)
    }

    fn snapshot(&self, depth: usize) -> Value {
        self.get().map_or(Value::Null, |value| value.snapshot(depth))
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        self.get().ok_or(Error::Empty { type_name: type_name::<Self>() })?.read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.get_mut().ok_or(Error::Empty { type_name: type_name::<Self>() })?.write(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.get().ok_or(Error::Empty { type_name: type_name::<Self>() })?.write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        self.get_mut().ok_or(Error::Empty { type_name: type_name::<Self>() })?.assign(value)
    }
}

// --- Locks -------------------------------------------------------------------------------------

fn acquire<G>(type_name: &'static str, mut attempt: impl FnMut() -> TryLockResult<G>) -> Result<G, Error> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match attempt() {
            Ok(guard) => return Ok(guard),
            // A poisoned lock still guards a value worth looking at: the state a panic left
            // behind is exactly what a debugging tool should be able to show.
            Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => thread::sleep(Duration::from_millis(1)),
            Err(TryLockError::WouldBlock) => return Err(Error::LockBusy { type_name }),
        }
    }
}

/// Transparent, and the point where shared access becomes exclusive: a write path locks here and
/// continues below with `&mut`, under the same lock the application itself uses.
impl<T: Reflect + ?Sized> Reflect for Mutex<T> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        acquire(self.type_name(), || self.try_lock()).map_or(Kind::Opaque, |guard| guard.kind())
    }

    fn members(&self) -> Vec<Member> {
        acquire(self.type_name(), || self.try_lock()).map(|guard| guard.members()).unwrap_or_default()
    }

    fn member_count(&self) -> usize {
        acquire(self.type_name(), || self.try_lock()).map_or(0, |guard| guard.member_count())
    }

    fn snapshot(&self, depth: usize) -> Value {
        match acquire(self.type_name(), || self.try_lock()) {
            Ok(guard) => guard.snapshot(depth),
            Err(_) => Value::String(format!("<locked: {}>", self.type_name())),
        }
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        acquire(self.type_name(), || self.try_lock())?.read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner()).write(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        acquire(self.type_name(), || self.try_lock())?.write(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        self.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner()).assign(value)
    }
}

/// Like `Mutex`, with reads under the read lock and writes under the write lock.
impl<T: Reflect + ?Sized> Reflect for RwLock<T> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        acquire(self.type_name(), || self.try_read()).map_or(Kind::Opaque, |guard| guard.kind())
    }

    fn members(&self) -> Vec<Member> {
        acquire(self.type_name(), || self.try_read()).map(|guard| guard.members()).unwrap_or_default()
    }

    fn member_count(&self) -> usize {
        acquire(self.type_name(), || self.try_read()).map_or(0, |guard| guard.member_count())
    }

    fn snapshot(&self, depth: usize) -> Value {
        match acquire(self.type_name(), || self.try_read()) {
            Ok(guard) => guard.snapshot(depth),
            Err(_) => Value::String(format!("<locked: {}>", self.type_name())),
        }
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        acquire(self.type_name(), || self.try_read())?.read(path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner()).write(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        acquire(self.type_name(), || self.try_write())?.write(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        self.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner()).assign(value)
    }
}

// --- Atomics -----------------------------------------------------------------------------------

/// A shared write loads the value, lets the visitor change a copy, and stores it back. That is
/// not a single atomic read-modify-write: a store the application makes in between is lost.
/// Acceptable for a debugging write; the app never sees a torn value.
macro_rules! atomic_impls {
    ($($atomic:ty => $plain:ty),*) => {$(
        impl Reflect for $atomic {
            fn type_name(&self) -> &'static str { stringify!($atomic) }
            fn kind(&self) -> Kind { Kind::Scalar }
            fn snapshot(&self, depth: usize) -> Value { self.load(Ordering::SeqCst).snapshot(depth) }

            fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
                if path.is_empty() { visit(self) } else { Err(Error::NotNavigable { type_name: self.type_name() }) }
            }

            fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
                if path.is_empty() { visit(self.get_mut()) } else { Err(Error::NotNavigable { type_name: self.type_name() }) }
            }

            fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
                if !path.is_empty() {
                    return Err(Error::NotNavigable { type_name: self.type_name() });
                }
                let mut value: $plain = self.load(Ordering::SeqCst);
                visit(&mut value)?;
                self.store(value, Ordering::SeqCst);
                Ok(())
            }

            fn assign(&mut self, value: &Value) -> Result<(), Error> {
                self.get_mut().assign(value)
            }
        }
    )*};
}

atomic_impls!(
    AtomicBool => bool,
    AtomicI32 => i32,
    AtomicI64 => i64,
    AtomicIsize => isize,
    AtomicU32 => u32,
    AtomicU64 => u64,
    AtomicUsize => usize
);
