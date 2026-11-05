//! `Reflect` for third-party locks.
//!
//! Locks are the one kind of type that cannot be handled generically (see [`crate::probes`]):
//! reading what one guards is only sound through its own locking API, and no std trait describes
//! that. So they are covered per abstraction rather than per crate:
//!
//! - `lock_api` (feature `lock_api`, on by default): `Mutex`, `RwLock` and `ReentrantMutex` of
//!   `parking_lot`, `spin` and every other crate built on it.
//! - `tokio::sync` (feature `tokio`): `Mutex` and `RwLock`. `cargo scry` turns the feature on by
//!   itself for an app that depends on tokio.
//!
//! They behave like std's locks: transparent, waited for at most a couple of seconds (never while
//! a hook captures), and the point where a write gets exclusive access.

#[allow(unused_imports)]
use serde_json::Value;

#[allow(unused_imports)]
use crate::impls::wait_for;
#[allow(unused_imports)]
use crate::{Error, Kind, Member, Reflect, Segment, Visit, VisitMut};

/// A `Reflect` impl for a lock, from how to try its read lock, its write lock (`None` for a lock
/// that only hands out shared access), and get exclusive access to its value.
#[allow(unused_macros)]
macro_rules! lock_impl {
    (
        [$($generics:tt)*] $lock:ty,
        read: |$r:ident| $read:expr,
        write: $(|$w:ident| $write:expr)?,
        get_mut: |$m:ident| $get_mut:expr $(,)?
    ) => {
        impl<$($generics)*> Reflect for $lock {
            fn type_name(&self) -> &'static str {
                ::std::any::type_name::<Self>()
            }

            fn kind(&self) -> Kind {
                let $r = self;
                wait_for(self.type_name(), || $read).map_or(Kind::Opaque, |guard| guard.kind())
            }

            fn members(&self) -> Vec<Member> {
                let $r = self;
                wait_for(self.type_name(), || $read).map(|guard| guard.members()).unwrap_or_default()
            }

            fn member_count(&self) -> usize {
                let $r = self;
                wait_for(self.type_name(), || $read).map_or(0, |guard| guard.member_count())
            }

            fn snapshot(&self, depth: usize) -> Value {
                let $r = self;
                match wait_for(self.type_name(), || $read) {
                    Ok(guard) => guard.snapshot(depth),
                    Err(_) => Value::String(format!("<locked: {}>", crate::reflect::short_type_name(self.type_name()))),
                }
            }

            fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
                let $r = self;
                wait_for(self.type_name(), || $read)?.read(path, visit)
            }

            fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
                let $m = self;
                $get_mut.write(path, visit)
            }

            fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
                $(
                    let $w = self;
                    return wait_for(self.type_name(), || $write)?.write(path, visit);
                )?
                #[allow(unreachable_code)]
                {
                    // Shared access only: a write has to find interior mutability further in.
                    let $r = self;
                    wait_for(self.type_name(), || $read)?.write_shared(path, visit)
                }
            }

            fn assign(&mut self, value: &Value) -> Result<(), Error> {
                let $m = self;
                $get_mut.assign(value)
            }
        }
    };
}

#[cfg(feature = "lock_api")]
lock_impl! {
    [R: lock_api::RawMutex, T: Reflect + ?Sized] lock_api::Mutex<R, T>,
    read: |lock| lock.try_lock(),
    write: |lock| lock.try_lock(),
    get_mut: |lock| lock.get_mut(),
}

#[cfg(feature = "lock_api")]
lock_impl! {
    [R: lock_api::RawRwLock, T: Reflect + ?Sized] lock_api::RwLock<R, T>,
    read: |lock| lock.try_read(),
    write: |lock| lock.try_write(),
    get_mut: |lock| lock.get_mut(),
}

#[cfg(feature = "lock_api")]
lock_impl! {
    [R: lock_api::RawMutex, G: lock_api::GetThreadId, T: Reflect + ?Sized] lock_api::ReentrantMutex<R, G, T>,
    read: |lock| lock.try_lock(),
    write:,
    get_mut: |lock| lock.get_mut(),
}

#[cfg(feature = "tokio")]
lock_impl! {
    [T: Reflect + ?Sized] tokio::sync::Mutex<T>,
    read: |lock| lock.try_lock().ok(),
    write: |lock| lock.try_lock().ok(),
    get_mut: |lock| lock.get_mut(),
}

#[cfg(feature = "tokio")]
lock_impl! {
    [T: Reflect + ?Sized] tokio::sync::RwLock<T>,
    read: |lock| lock.try_read().ok(),
    write: |lock| lock.try_write().ok(),
    get_mut: |lock| lock.get_mut(),
}
