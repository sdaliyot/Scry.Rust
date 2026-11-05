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

    /// Replaces this value from JSON. Structs take an object of the fields to change; scalars and
    /// unit enum variants take a JSON scalar.
    fn assign(&mut self, value: &Value) -> Result<(), Error> {
        let _ = value;
        Err(Error::not_writable(format!("{} cannot be assigned from JSON", self.type_name())))
    }
}
