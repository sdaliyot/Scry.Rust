//! `Reflect` for Dioxus signals.
//!
//! Signals belong to the Dioxus runtime on the UI thread: these impls must be called from there,
//! and a `GlobalSignal` panics if resolved anywhere else. Writes go through the signal, so the
//! components subscribed to it re-render exactly as if the app had written it.

use std::any::type_name;

use dioxus_signals::{GlobalSignal, ReadableExt, Signal, WritableExt};
use serde_json::Value;

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

impl<T: Reflect + 'static> Reflect for GlobalSignal<T> {
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }

    fn kind(&self) -> Kind {
        self.signal().kind()
    }

    fn members(&self) -> Vec<Member> {
        self.signal().members()
    }

    fn snapshot(&self, depth: usize) -> Value {
        self.signal().snapshot(depth)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        // Fully qualified: Dioxus's ReadableExt::read applies to a Signal as well.
        Reflect::read(&self.signal(), path, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.signal().write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.signal().write_shared(path, visit)
    }

    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        self.signal().assign(value)
    }
}
