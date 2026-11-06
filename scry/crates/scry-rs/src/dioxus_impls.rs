//! `Reflect` for Dioxus signals, resources and actions.
//!
//! Signals belong to the Dioxus runtime on the UI thread: these impls must be called from there,
//! and a `GlobalSignal` reports `no_runtime` anywhere else. Writes go through the signal, so the
//! components subscribed to it re-render exactly as if the app had written it.

use std::any::type_name;

use dioxus_hooks::{Action, Resource, UseResourceState};
use dioxus_signals::{GlobalSignal, Memo, ReadableExt, Signal, WritableExt};
use serde_json::{Value, json};

use crate::{Error, Kind, Member, Reflect, Segment, Visit, VisitMut};

impl<T: Reflect + 'static> Reflect for Signal<T> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        self.try_read().map_or(Kind::Opaque, |value| value.kind())
    }

    fn members(&self) -> Vec<Member> {
        self.try_read().map(|value| value.members()).unwrap_or_default()
    }

    fn member_count(&self) -> usize {
        self.try_read().map_or(0, |value| value.member_count())
    }

    fn snapshot(&self, depth: usize) -> Value {
        match self.try_read() {
            Ok(value) => value.snapshot(depth),
            Err(_) => Value::String(format!("<borrowed: {}>", self.type_name())),
        }
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        let value = self.try_read().map_err(|_| Error::LockBusy { type_name: self.type_name() })?;
        value.read(path, visit)
    }

    /// A signal is a copyable handle, so `&mut` adds nothing over `&`.
    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let mut handle = *self;
        let mut value = handle.try_write().map_err(|_| Error::LockBusy { type_name: type_name::<Self>() })?;
        value.write(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        self.write_shared(&[], &mut |target| target.assign(value))
    }
}

/// Read-only: a memo's value is recomputed from its inputs, so writing it would not stick.
impl<T: Reflect + PartialEq + 'static> Reflect for Memo<T> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        self.try_read().map_or(Kind::Opaque, |value| value.kind())
    }

    fn members(&self) -> Vec<Member> {
        self.try_read().map(|value| value.members()).unwrap_or_default()
    }

    fn member_count(&self) -> usize {
        self.try_read().map_or(0, |value| value.member_count())
    }

    fn snapshot(&self, depth: usize) -> Value {
        match self.try_read() {
            Ok(value) => value.snapshot(depth),
            Err(_) => Value::String(format!("<borrowed: {}>", self.type_name())),
        }
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        let value = self.try_read().map_err(|_| Error::LockBusy { type_name: self.type_name() })?;
        value.read(path, visit)
    }

    fn write(&mut self, _path: &[Segment], _visit: VisitMut<'_>) -> Result<(), Error> {
        Err(Error::not_writable("a memo is derived from other state; change its inputs instead"))
    }

    fn write_shared(&self, _path: &[Segment], _visit: VisitMut<'_>) -> Result<(), Error> {
        Err(Error::not_writable("a memo is derived from other state; change its inputs instead"))
    }
}

impl<T: Reflect + 'static> Reflect for GlobalSignal<T> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        resolve(self).map_or(Kind::Opaque, |signal| signal.kind())
    }

    fn members(&self) -> Vec<Member> {
        resolve(self).map(|signal| signal.members()).unwrap_or_default()
    }

    fn member_count(&self) -> usize {
        resolve(self).map_or(0, |signal| signal.member_count())
    }

    fn snapshot(&self, depth: usize) -> Value {
        match resolve(self) {
            Ok(signal) => signal.snapshot(depth),
            Err(error) => Value::String(format!("<{error}>")),
        }
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        // Fully qualified: Dioxus's ReadableExt::read applies to a Signal as well.
        Reflect::read(&resolve(self)?, path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        resolve(self)?.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        resolve(self)?.write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        resolve(self)?.assign(value)
    }
}

/// The signal behind a global one. Resolving it needs the Dioxus runtime, and panics without one -
/// on a thread other than the UI thread, or after the app's window has closed.
fn resolve<T: 'static>(global: &GlobalSignal<T>) -> Result<Signal<T>, Error> {
    match dioxus_core::Runtime::try_current() {
        Some(_) => Ok(global.signal()),
        None => Err(Error::NoRuntime { type_name: type_name::<GlobalSignal<T>>() }),
    }
}

// ---- Resources and actions ----

/// A `use_resource` handle: `state` (`Pending`, `Ready`, `Paused`, `Stopped`) and the `value` its
/// future produced, if it has finished. Read-only; `restart`, `cancel`, `pause`, `resume` and
/// `clear` are callable on it, as the app's own code would call them.
impl<T: Reflect + 'static> Reflect for Resource<T> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        Kind::Struct
    }

    fn members(&self) -> Vec<Member> {
        vec![Member::new("state", type_name::<UseResourceState>()), Member::new("value", type_name::<Option<T>>())]
    }

    fn snapshot(&self, depth: usize) -> Value {
        if depth == 0 {
            return crate::__private::truncated(self.type_name());
        }
        let value = self.value();
        let value = match value.try_read() {
            Ok(value) => value.snapshot(depth - 1),
            Err(_) => Value::String("<borrowed>".into()),
        };
        json!({ "state": resource_state(self), "value": value })
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else {
            return visit(self);
        };
        match segment.field() {
            Some("state") => resource_state(self).read(rest, visit),
            Some("value") => {
                let value = self.value();
                let value = value.try_read().map_err(|_| Error::LockBusy { type_name: self.type_name() })?;
                value.read(rest, visit)
            }
            _ => Err(crate::__private::no_member(self.type_name(), segment)),
        }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, _path: &[Segment], _visit: VisitMut<'_>) -> Result<(), Error> {
        Err(Error::not_writable("a resource's value comes from its future; call restart() to run it again"))
    }

    fn builtin_methods(&self) -> Vec<&'static str> {
        vec![
            "fn restart(&mut self)  // cancels the future and runs it again",
            "fn cancel(&mut self)  // stops the future; state becomes Stopped",
            "fn pause(&mut self)",
            "fn resume(&mut self)",
            "fn clear(&mut self)  // forgets the value",
        ]
    }

    fn call_builtin(&self, name: &str, args: &[Value]) -> Option<Result<Value, Error>> {
        let mut handle = *self;
        let run: fn(&mut Resource<T>) = match name {
            "restart" => Resource::restart,
            "cancel" => Resource::cancel,
            "pause" => Resource::pause,
            "resume" => Resource::resume,
            "clear" => Resource::clear,
            _ => return None,
        };
        Some(no_arguments(name, args).map(|()| {
            run(&mut handle);
            Value::String(resource_state(&handle))
        }))
    }
}

fn resource_state<T>(resource: &Resource<T>) -> String {
    let state = resource.state();
    state.try_read().map_or_else(|_| "<borrowed>".to_owned(), |state| format!("{:?}", *state))
}

/// A `use_action` handle: whether it is `pending`, and the `value` of its last run - its result,
/// or `{ "error": ... }` when that failed. Read-only; `reset` and `cancel` are callable on it.
impl<I: 'static, O: Reflect + 'static> Reflect for Action<I, O> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        Kind::Struct
    }

    fn members(&self) -> Vec<Member> {
        vec![Member::new("pending", "bool"), Member::new("value", type_name::<O>())]
    }

    fn snapshot(&self, depth: usize) -> Value {
        if depth == 0 {
            return crate::__private::truncated(self.type_name());
        }
        let value = match self.value() {
            None => Value::Null,
            Some(Ok(value)) => {
                value.try_read().map_or(Value::String("<borrowed>".into()), |value| value.snapshot(depth - 1))
            }
            Some(Err(error)) => json!({ "error": error.to_string() }),
        };
        json!({ "pending": self.pending(), "value": value })
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else {
            return visit(self);
        };
        match segment.field() {
            Some("pending") => self.pending().read(rest, visit),
            Some("value") => match self.value() {
                None => None::<()>.read(rest, visit),
                Some(Ok(value)) => {
                    let value = value.try_read().map_err(|_| Error::LockBusy { type_name: self.type_name() })?;
                    value.read(rest, visit)
                }
                Some(Err(error)) => error.to_string().read(rest, visit),
            },
            _ => Err(crate::__private::no_member(self.type_name(), segment)),
        }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, _path: &[Segment], _visit: VisitMut<'_>) -> Result<(), Error> {
        Err(Error::not_writable("an action's value comes from its last run; reset() clears it"))
    }

    fn builtin_methods(&self) -> Vec<&'static str> {
        let mut methods = vec![
            "fn reset(&mut self)  // forgets the last run's value",
            "fn cancel(&mut self)  // stops a run in progress",
        ];
        if self.without_input().is_some() {
            methods.insert(0, "fn call(&mut self)  // runs the action, as the app's own call() does");
        }
        methods
    }

    fn call_builtin(&self, name: &str, args: &[Value]) -> Option<Result<Value, Error>> {
        let mut handle = *self;
        let run: fn(&mut Action<I, O>) = match name {
            "call" => {
                let mut action = self.without_input()?;
                return Some(no_arguments(name, args).map(|()| {
                    // The action runs as a task of its own; this only starts it.
                    drop(action.call());
                    Value::Null
                }));
            }
            "reset" => Action::reset,
            "cancel" => Action::cancel,
            _ => return None,
        };
        Some(no_arguments(name, args).map(|()| {
            run(&mut handle);
            Value::Null
        }))
    }
}

impl<I: 'static, O: 'static> ActionInput<O> for Action<I, O> {
    fn without_input(&self) -> Option<Action<(), O>> {
        (self as &dyn std::any::Any).downcast_ref::<Action<(), O>>().copied()
    }
}

/// An action whose closure takes no arguments, which Scry can run without building any.
trait ActionInput<O> {
    fn without_input(&self) -> Option<Action<(), O>>;
}

fn no_arguments(name: &str, args: &[Value]) -> Result<(), Error> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(Error::ArgumentCount { function: name.to_owned(), expected: 0, found: args.len() })
    }
}
