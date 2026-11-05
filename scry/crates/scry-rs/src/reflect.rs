use std::any::Any;

use serde_json::Value;

use crate::{Error, Segment};

/// Called with the value a read path resolved to. Any guard taken along the way is held for the
/// duration of the call.
pub type Visit<'f> = &'f mut dyn FnMut(&dyn Reflect) -> Result<(), Error>;

/// Called with exclusive access to the value a write path resolved to.
pub type VisitMut<'f> = &'f mut dyn FnMut(&mut dyn Reflect) -> Result<(), Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Scalar,
    Struct,
    Enum,
    Sequence,
    Tuple,
    /// Shown through `Debug` or as a type name; neither navigable nor writable.
    Opaque,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Scalar => "scalar",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Sequence => "sequence",
            Kind::Tuple => "tuple",
            Kind::Opaque => "opaque",
        }
    }
}

/// A type name without its module paths, for display: `Vec<Item>` for
/// `alloc::vec::Vec<console_demo::inventory::Item>`, `dyn PriceRule + Send` for
/// `dyn console_demo::pricing::PriceRule + Send`. Two types can share a short name, so anything
/// that needs to tell them apart keeps the full one.
pub fn short_type_name(full: &str) -> String {
    let mut out = String::with_capacity(full.len());
    let mut segment = String::new();
    let mut chars = full.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            segment.push(c);
        } else if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            // A path segment: drop it, keeping only what follows the last `::`.
            if segment.is_empty() {
                out.push_str("::");
            }
            segment.clear();
        } else {
            out.push_str(&segment);
            segment.clear();
            out.push(c);
        }
    }
    out.push_str(&segment);
    out
}

/// A navigable child: a field name or index, and the type declared for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub type_name: &'static str,
}

impl Member {
    pub fn new(name: impl Into<String>, type_name: &'static str) -> Self {
        Self { name: name.into(), type_name }
    }
}

/// Runtime access to a value's structure. Implemented by `#[derive(Reflect)]` for app types and by
/// this crate for std types; wrappers (`Box`, `Arc`, `OnceLock`, locks, signals) are transparent,
/// so they never appear as a path segment.
pub trait Reflect {
    fn type_name(&self) -> &'static str;

    fn kind(&self) -> Kind;

    /// Direct children, for listing. Wrappers report their inner value's members.
    fn members(&self) -> Vec<Member> {
        Vec::new()
    }

    /// How many children this value has. Unlike [`Reflect::members`], which lists at most 100
    /// elements of a collection, this is the full count.
    fn member_count(&self) -> usize {
        self.members().len()
    }

    /// A JSON view of the value, `depth` levels deep. Deeper values are replaced by a
    /// `"<TypeName>"` placeholder, so a snapshot of a large graph stays bounded.
    fn snapshot(&self, depth: usize) -> Value;

    /// Resolves `path` relative to this value and calls `visit` on the result.
    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error>;

    /// Resolves `path` with exclusive access and calls `visit` on the result.
    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error>;

    /// Resolves `path` for writing from shared access. Only interior-mutable values (locks,
    /// atomics, signals) can turn shared access into exclusive access; containers forward the call
    /// to the child named by the path, and everything else refuses.
    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let _ = (path, visit);
        Err(Error::needs_interior_mutability(self.type_name()))
    }

    /// This value as `Any`, so a method can be called on it. Given by `#[derive(Reflect)]` for
    /// types without generic parameters; other values have no callable methods.
    fn as_any(&self) -> Option<&dyn Any> {
        None
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        None
    }

    /// A new value built from JSON, for when there is none to assign into yet - an `Option` that is
    /// `None`. Given for std scalars and strings, and by `#[derive(Reflect)]` for types without
    /// generic parameters.
    fn build(value: &Value) -> Result<Self, crate::Error>
    where
        Self: Sized,
    {
        let _ = value;
        Err(crate::Error::CannotConvert { type_name: std::any::type_name::<Self>() })
    }

    /// Replaces this value from JSON. Structs take an object of the fields to change; scalars and
    /// unit enum variants take a JSON scalar.
    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        let _ = value;
        Err(Error::not_writable(format!("{} cannot be assigned from JSON", self.type_name())))
    }
}

#[cfg(test)]
mod tests {
    use super::short_type_name;

    #[test]
    fn short_type_names_drop_module_paths() {
        assert_eq!(short_type_name("alloc::vec::Vec<console_demo::inventory::Item>"), "Vec<Item>");
        assert_eq!(
            short_type_name("std::sync::Mutex<alloc::boxed::Box<dyn app::Rule + core::marker::Send>>"),
            "Mutex<Box<dyn Rule + Send>>"
        );
        assert_eq!(short_type_name("(u32, &str, [u8; 4])"), "(u32, &str, [u8; 4])");
        assert_eq!(short_type_name("<app::A as app::T>::Out"), "<A as T>::Out");
        assert_eq!(short_type_name("app::f::{{closure}}"), "{{closure}}");
    }
}
