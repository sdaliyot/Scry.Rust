//! Scry.Rust: inspect and change the state of a running Rust process.
//!
//! A [`Registry`] holds named roots - statics, shared app state, UI signals - and resolves paths
//! such as `APP.inventory.items[2].qty` against them through the [`Reflect`] trait. Fields whose
//! types do not implement it still show up, through their `Debug` output or as an opaque type
//! name, so reflecting a type never fails because of a third-party field.
//!
//! An app normally never mentions this crate in its code. It declares an optional dependency and
//! a `scry` feature, and is built with `cargo scry` (the `scry-rs-cargo` crate), which compiles a
//! rewritten copy of its sources: `#[derive(Reflect)]` on every type, a registration for every
//! static, reflection for trait objects and Dioxus hooks, and an [`endpoint`] started from `main`
//! for the `scry` CLI. The derive and [`Registry`] can also be used by hand.
//!
//! Two rules shape the API, both forced by Rust rather than chosen:
//!
//! - **Access is callback-based.** A value behind a `Mutex`, `RwLock` or signal cannot be returned
//!   by reference past its guard, so every read and write runs inside a callback while the guard
//!   is held, and the guard is released as soon as the callback returns.
//! - **Writes go through the app's own synchronization.** The registry only ever has shared
//!   access to its roots, so a write must pass a lock, an atomic or a signal somewhere along its
//!   path; from there on it has exclusive access. A path with none of those is refused rather than
//!   written through an aliased reference, which would be undefined behavior.

mod command;
pub mod endpoint;
mod error;
mod impls;
mod path;
mod reflect;
mod registry;

#[cfg(feature = "dioxus")]
#[doc(hidden)]
pub mod dioxus_hooks;
#[cfg(feature = "dioxus")]
mod dioxus_impls;

#[doc(hidden)]
pub mod __private;

pub use command::execute;
pub use error::Error;
pub use path::Segment;
pub use reflect::{Kind, Member, Reflect, Visit, VisitMut};
pub use registry::Registry;
pub use scry_rs_derive::Reflect;
pub use serde_json::Value;
