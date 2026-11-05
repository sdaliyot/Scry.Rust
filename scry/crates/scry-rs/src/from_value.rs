//! Building argument values from JSON, for calls into the app.
//!
//! [`FromValue`] is implemented here for std types and by `#[derive(Reflect)]` for app types.
//! Generated code reaches it through the `probe_arg!` and `probe_ref_arg!` probes (see
//! [`crate::probes`]), which fall back to `Deserialize` and `FromStr` for types without it, so a
//! parameter type that supports none of them still compiles and only fails when called.

use std::any::type_name;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::Error;

/// A value that can be built from JSON, the way `assign` would write it.
pub trait FromValue: Sized {
    fn from_value(value: &Value) -> Result<Self, Error>;
}

macro_rules! integer_from_value {
    ($($ty:ty),*) => {$(
        impl FromValue for $ty {
            fn from_value(value: &Value) -> Result<Self, Error> {
                let number = value.as_i64().map(i128::from).or_else(|| value.as_u64().map(i128::from));
                number.and_then(|number| <$ty>::try_from(number).ok()).ok_or_else(|| Error::mismatch(stringify!($ty), value))
            }
        }
    )*};
}

integer_from_value!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize);

impl FromValue for f64 {
    fn from_value(value: &Value) -> Result<Self, Error> {
        value.as_f64().ok_or_else(|| Error::mismatch("f64", value))
    }
}

impl FromValue for f32 {
    fn from_value(value: &Value) -> Result<Self, Error> {
        value.as_f64().map(|value| value as f32).ok_or_else(|| Error::mismatch("f32", value))
    }
}

impl FromValue for bool {
    fn from_value(value: &Value) -> Result<Self, Error> {
        value.as_bool().ok_or_else(|| Error::mismatch("bool", value))
    }
}

impl FromValue for char {
    fn from_value(value: &Value) -> Result<Self, Error> {
        let mut chars = value.as_str().unwrap_or_default().chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Ok(c),
            _ => Err(Error::mismatch("a one-character string", value)),
        }
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Result<Self, Error> {
        value.as_str().map(str::to_owned).ok_or_else(|| Error::mismatch("a string", value))
    }
}

impl FromValue for PathBuf {
    fn from_value(value: &Value) -> Result<Self, Error> {
        value.as_str().map(PathBuf::from).ok_or_else(|| Error::mismatch("a path string", value))
    }
}

impl FromValue for () {
    fn from_value(_value: &Value) -> Result<Self, Error> {
        Ok(())
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Result<Self, Error> {
        if value.is_null() { Ok(None) } else { T::from_value(value).map(Some) }
    }
}

/// `{"Ok": value}` or `{"Err": error}`, the way a `Result` snapshot shows it.
impl<T: FromValue, E: FromValue> FromValue for Result<T, E> {
    fn from_value(value: &Value) -> Result<Self, Error> {
        let expected = "{\"Ok\": ..} or {\"Err\": ..}";
        let object =
            value.as_object().filter(|object| object.len() == 1).ok_or_else(|| Error::mismatch(expected, value))?;
        match object.iter().next() {
            Some((key, inner)) if key == "Ok" => T::from_value(inner).map(Ok),
            Some((key, inner)) if key == "Err" => E::from_value(inner).map(Err),
            _ => Err(Error::mismatch(expected, value)),
        }
    }
}

impl<T: FromValue> FromValue for Box<T> {
    fn from_value(value: &Value) -> Result<Self, Error> {
        T::from_value(value).map(Box::new)
    }
}

impl<T: FromValue> FromValue for Arc<T> {
    fn from_value(value: &Value) -> Result<Self, Error> {
        T::from_value(value).map(Arc::new)
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(value: &Value) -> Result<Self, Error> {
        value.as_array().ok_or_else(|| Error::mismatch("an array", value))?.iter().map(T::from_value).collect()
    }
}

macro_rules! map_from_value {
    ($map:ident, $($bound:path),*) => {
        /// Keys come from the object's member names, read as JSON strings.
        impl<K: FromValue $(+ $bound)*, V: FromValue> FromValue for $map<K, V> {
            fn from_value(value: &Value) -> Result<Self, Error> {
                let object = value.as_object().ok_or_else(|| Error::mismatch("an object", value))?;
                object.iter().map(|(key, value)| Ok((K::from_value(&Value::String(key.clone()))?, V::from_value(value)?))).collect()
            }
        }
    };
}

map_from_value!(HashMap, Eq, Hash);
map_from_value!(BTreeMap, Ord);

macro_rules! tuple_from_value {
    ($($len:literal: $($name:ident $index:tt),+;)*) => {$(
        /// A JSON array of exactly this many elements.
        impl<$($name: FromValue),+> FromValue for ($($name,)+) {
            fn from_value(value: &Value) -> Result<Self, Error> {
                match value.as_array() {
                    Some(items) if items.len() == $len => Ok(($($name::from_value(&items[$index])?,)+)),
                    _ => Err(Error::mismatch(concat!("an array of ", $len), value)),
                }
            }
        }
    )*};
}

tuple_from_value! {
    1: A 0;
    2: A 0, B 1;
    3: A 0, B 1, C 2;
    4: A 0, B 1, C 2, D 3;
}

/// Picks how a method taking `self` by value gets its receiver: a clone of the live value when
/// `T: Clone`, so the app's own value is left alone; otherwise the call is refused.
pub struct SelfProbe<'a, T>(pub &'a T);

pub trait ViaClone<T> {
    fn scry_self(&self) -> Result<T, Error>;
}

impl<T: Clone> ViaClone<T> for &&SelfProbe<'_, T> {
    fn scry_self(&self) -> Result<T, Error> {
        Ok(self.0.clone())
    }
}

pub trait ViaNoClone<T> {
    fn scry_self(&self) -> Result<T, Error>;
}

impl<T> ViaNoClone<T> for &SelfProbe<'_, T> {
    fn scry_self(&self) -> Result<T, Error> {
        Err(Error::NotCallable(format!("the method takes self by value, and {} is not Clone", type_name::<T>())))
    }
}
