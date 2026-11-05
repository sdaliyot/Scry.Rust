//! Calling the app's functions and methods.
//!
//! `cargo scry` emits one [`Function`] next to every function and method it can call - see the
//! generator for which ones - into [`FUNCTIONS`](crate::__private::FUNCTIONS). Each holds a shim
//! that builds the arguments from JSON, makes the call and snapshots the result.
//!
//! A method runs on a live value: the registry resolves the path, and the method whose `Self` is
//! that value's type (through [`Reflect::as_any`]) is called on it while any guard on the way is
//! held. A `&mut self` method needs the same lock, atomic or signal on its path as a write does.
//! A panic in the called code is caught and reported as `panicked`.
//!
//! The called code runs on the registry's thread with its guards held. A method that locks a
//! mutex its own path already passed through - a `Mutex` is not reentrant - blocks that thread.

use std::any::{Any, TypeId};
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::{Value, json};

use crate::{Error, Reflect, Registry, Segment};

/// One callable function or method, emitted by the generator.
pub struct Function {
    /// The name methods are called by (`total_value`).
    pub name: &'static str,
    /// The full path (`console_demo::inventory::Inventory::total_value`); for a trait method,
    /// `<console_demo::pricing::Discount as PriceRule>::apply`.
    pub path: &'static str,
    /// The signature as written in the source.
    pub signature: &'static str,
    pub receiver: Receiver,
    /// `Self` of a method or associated function.
    pub self_type: Option<fn() -> TypeId>,
    /// Set for a method of a trait impl.
    pub trait_name: Option<&'static str>,
    pub arity: usize,
    pub invoke: fn(This<'_>, &[Value], usize) -> Result<Value, Error>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receiver {
    /// A free function, or an associated function without `self`.
    None,
    Ref,
    Mut,
}

/// The value a method is called on.
pub enum This<'a> {
    None,
    Ref(&'a dyn Any),
    Mut(&'a mut dyn Any),
}

impl<'a> This<'a> {
    pub fn downcast_ref<T: Any>(self) -> Result<&'a T, Error> {
        match self {
            This::Ref(value) => value.downcast_ref(),
            This::Mut(value) => value.downcast_mut().map(|value| &*value),
            This::None => None,
        }
        .ok_or_else(|| Error::Script(format!("internal: the receiver is not a {}", std::any::type_name::<T>())))
    }

    pub fn downcast_mut<T: Any>(self) -> Result<&'a mut T, Error> {
        match self {
            This::Mut(value) => value.downcast_mut(),
            _ => None,
        }
        .ok_or_else(|| Error::Script(format!("internal: the receiver is not a mutable {}", std::any::type_name::<T>())))
    }
}

fn all() -> &'static [Function] {
    &crate::__private::FUNCTIONS
}

/// Every function, as `{path, signature}` objects; with a `filter`, those whose path contains it.
pub(crate) fn list(filter: &str) -> Value {
    let filter = filter.to_lowercase();
    let mut functions: Vec<&Function> =
        all().iter().filter(|function| function.path.to_lowercase().contains(&filter)).collect();
    functions.sort_by_key(|function| function.path);
    Value::Array(
        functions.iter().map(|function| json!({ "path": function.path, "signature": function.signature })).collect(),
    )
}

/// The signatures of the methods callable on `value`.
pub(crate) fn methods_of(value: &dyn Reflect) -> Vec<&'static str> {
    let Some(type_id) = value.as_any().map(|value| value.type_id()) else { return Vec::new() };
    let mut methods: Vec<&Function> = methods(type_id).collect();
    methods.sort_by_key(|function| function.name);
    methods.iter().map(|function| function.signature).collect()
}

fn methods(type_id: TypeId) -> impl Iterator<Item = &'static Function> {
    all().iter().filter(move |function| {
        function.receiver != Receiver::None && function.self_type.is_some_and(|self_type| self_type() == type_id)
    })
}

/// A free or associated function by its full path, or by a unique `::`-suffix of it. An
/// instantiation of a generic function (`model::ratio::<u32>`) also answers to its path without
/// the type arguments, when it is the only one.
fn free(name: &str) -> Result<&'static Function, Error> {
    let candidates: Vec<&Function> = all().iter().filter(|function| function.receiver == Receiver::None).collect();
    if let Some(function) = candidates.iter().find(|function| function.path == name) {
        return Ok(function);
    }
    let suffix = format!("::{name}");
    let mut matches: Vec<&Function> =
        candidates.iter().copied().filter(|function| function.path.ends_with(&suffix)).collect();
    if matches.is_empty() {
        let base = |function: &&Function| function.path.split("::<").next().unwrap_or(function.path).to_owned();
        matches = candidates
            .into_iter()
            .filter(|function| base(function) == name || base(function).ends_with(&suffix))
            .collect();
    }
    match matches.as_slice() {
        [function] => Ok(function),
        [] => Err(Error::NoSuchFunction(name.to_owned())),
        _ => Err(Error::AmbiguousFunction {
            name: name.to_owned(),
            candidates: matches.iter().map(|function| function.path.to_owned()).collect(),
        }),
    }
}

/// The short names of free functions that are unique, for scripts to call directly.
#[cfg_attr(not(feature = "script"), allow(dead_code))]
pub(crate) fn free_names() -> Vec<(&'static str, usize)> {
    let free: Vec<&Function> =
        all().iter().filter(|function| function.receiver == Receiver::None && function.self_type.is_none()).collect();
    free.iter()
        .filter(|function| free.iter().filter(|other| other.name == function.name).count() == 1)
        .map(|function| (function.name, function.arity))
        .collect()
}

/// Every method name with its arity, for scripts to call with method syntax.
#[cfg_attr(not(feature = "script"), allow(dead_code))]
pub(crate) fn method_names() -> Vec<(&'static str, usize)> {
    let mut names: Vec<(&str, usize)> = all()
        .iter()
        .filter(|function| function.receiver != Receiver::None)
        .map(|function| (function.name, function.arity))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

fn invoke(function: &Function, this: This<'_>, args: &[Value], depth: usize) -> Result<Value, Error> {
    if args.len() != function.arity {
        return Err(Error::ArgumentCount {
            function: function.path.to_owned(),
            expected: function.arity,
            found: args.len(),
        });
    }
    catch_unwind(AssertUnwindSafe(|| (function.invoke)(this, args, depth))).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|message| message.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a non-text panic payload".into());
        Err(Error::Panicked(format!("{}: {message}", function.path)))
    })
}

/// Calls the free function `name`.
pub(crate) fn call_free(name: &str, args: &[Value], depth: usize) -> Result<Value, Error> {
    invoke(free(name)?, This::None, args, depth)
}

impl Registry {
    /// Calls a function or method. `target` is a function's full path or a unique `::`-suffix of
    /// it (`util::ratio`), or a value's path followed by a method name (`APP.inventory.total_value`).
    /// Arguments are JSON; the result is a snapshot `depth` levels deep.
    pub fn call(&self, target: &str, args: &[Value], depth: usize) -> Result<Value, Error> {
        let (root, mut segments) = crate::path::parse(target)?;
        match segments.pop() {
            None => call_free(&root, args, depth),
            Some(Segment::Field(method)) => self.call_method_at(&root, &segments, &method, args, depth),
            Some(Segment::Index(index)) => Err(Error::BadPath(format!("'[{index}]' is not a method name"))),
        }
    }

    /// Calls `method` on the value `segments` below `root` resolve to.
    pub(crate) fn call_method_at(
        &self,
        root: &str,
        segments: &[Segment],
        method: &str,
        args: &[Value],
        depth: usize,
    ) -> Result<Value, Error> {
        let mut found = None;
        self.read_at(root, segments, &mut |value| {
            found = Some((value.as_any().map(|value| value.type_id()), value.type_name()));
            Ok(())
        })?;
        let Some((type_id, type_name)) = found else {
            return Err(Error::NoSuchMethod { type_name: "?", method: method.into() });
        };
        let no_method = || Error::NoSuchMethod { type_name, method: method.to_owned() };
        let candidates: Vec<&Function> =
            methods(type_id.ok_or_else(no_method)?).filter(|function| function.name == method).collect();
        let function = match candidates.as_slice() {
            [] => return Err(no_method()),
            [function] => *function,
            many => match many.iter().filter(|function| function.trait_name.is_none()).collect::<Vec<_>>().as_slice() {
                [inherent] => **inherent,
                _ => {
                    return Err(Error::AmbiguousFunction {
                        name: method.to_owned(),
                        candidates: many.iter().map(|function| function.path.to_owned()).collect(),
                    });
                }
            },
        };

        let mut result = None;
        match function.receiver {
            Receiver::Ref => self.read_at(root, segments, &mut |value| {
                let this = value.as_any().ok_or_else(no_method)?;
                result = Some(invoke(function, This::Ref(this), args, depth));
                Ok(())
            })?,
            Receiver::Mut => self.write_shared_at(root, segments, &mut |value| {
                let this = value.as_any_mut().ok_or_else(no_method)?;
                result = Some(invoke(function, This::Mut(this), args, depth));
                Ok(())
            })?,
            Receiver::None => return Err(no_method()),
        }
        result.unwrap_or_else(|| Err(no_method()))
    }
}
