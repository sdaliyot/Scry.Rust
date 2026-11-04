use serde_json::{Map, Value, json};

use crate::__private::FieldRef;
use crate::path::parse;
use crate::{Error, Member, Reflect, Segment};

/// Named roots that paths start from.
///
/// Deliberately not `Send`: roots may be UI-thread values such as Dioxus signals, so a registry
/// lives on the thread that owns its roots, and other threads reach it by sending it requests.
#[derive(Default)]
pub struct Registry {
    roots: Vec<(String, Root)>,
}

enum Root {
    Owned(Box<dyn Reflect>),
    Static(&'static dyn Reflect),
    /// A generated static, whose type may not implement `Reflect` at all.
    Field(FieldRef<'static>),
}

impl Root {
    fn get(&self) -> &dyn Reflect {
        match self {
            Root::Owned(value) => value.as_ref(),
            Root::Static(value) => *value,
            Root::Field(value) => value.get(),
        }
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry holding every root the scry generator found: each `static` in the crates it
    /// rewrote, named by its full path (`console_demo::APP`), and with the `dioxus` feature the
    /// signals of mounted components under `ui`. Paths may use a static's short name when it is
    /// unique (`APP.inventory`).
    ///
    /// Without the generator - a plain `--features scry` build - this is just an empty registry.
    pub fn discovered() -> Self {
        let mut registry = Self::new();
        for register in crate::__private::STATICS {
            register(&mut registry);
        }
        registry.roots.sort_by(|(a, _), (b, _)| a.cmp(b));
        #[cfg(feature = "dioxus")]
        registry.add("ui", crate::dioxus_hooks::Ui);
        registry
    }

    #[doc(hidden)]
    pub fn __add_field(&mut self, name: &str, root: FieldRef<'static>) {
        self.insert(name.to_owned(), Root::Field(root));
    }

    /// Adds a root the registry owns: an `Arc` to shared state, a signal handle. Replaces any
    /// root of the same name.
    pub fn add(&mut self, name: impl Into<String>, root: impl Reflect + 'static) -> &mut Self {
        self.insert(name.into(), Root::Owned(Box::new(root)))
    }

    /// Adds a `static` - the most common root, and the only one reachable without the app's help.
    pub fn add_static(&mut self, name: impl Into<String>, root: &'static dyn Reflect) -> &mut Self {
        self.insert(name.into(), Root::Static(root))
    }

    fn insert(&mut self, name: String, root: Root) -> &mut Self {
        self.roots.retain(|(existing, _)| *existing != name);
        self.roots.push((name, root));
        self
    }

    pub fn roots(&self) -> Vec<Member> {
        self.roots.iter().map(|(name, root)| Member::new(name.clone(), root.get().type_name())).collect()
    }

    fn resolve(&self, path: &str) -> Result<(&dyn Reflect, Vec<Segment>), Error> {
        let (root, segments) = parse(path)?;
        Ok((self.find(&root)?.get(), segments))
    }

    /// An exact root name, or the unique one ending in `::name`.
    fn find(&self, root: &str) -> Result<&Root, Error> {
        if let Some((_, value)) = self.roots.iter().find(|(name, _)| name == root) {
            return Ok(value);
        }
        let suffix = format!("::{root}");
        let matches: Vec<&(String, Root)> = self.roots.iter().filter(|(name, _)| name.ends_with(&suffix)).collect();
        match matches.as_slice() {
            [(_, value)] => Ok(value),
            [] => Err(Error::NoSuchRoot(root.to_owned())),
            _ => Err(Error::AmbiguousRoot {
                name: root.to_owned(),
                candidates: matches.iter().map(|(name, _)| name.clone()).collect(),
            }),
        }
    }

    /// A JSON snapshot of the value at `path`, `depth` levels deep.
    pub fn get(&self, path: &str, depth: usize) -> Result<Value, Error> {
        let (root, segments) = self.resolve(path)?;
        let mut snapshot = Value::Null;
        root.read(&segments, &mut |value| {
            snapshot = value.snapshot(depth);
            Ok(())
        })?;
        Ok(snapshot)
    }

    /// The type, kind and members of the value at `path`.
    pub fn describe(&self, path: &str) -> Result<Value, Error> {
        let (root, segments) = self.resolve(path)?;
        let mut description = Value::Null;
        root.read(&segments, &mut |value| {
            let members: Vec<Value> =
                value.members().into_iter().map(|member| json!({ "name": member.name, "type": member.type_name })).collect();
            let mut object = Map::new();
            object.insert("path".into(), Value::String(path.trim().to_owned()));
            object.insert("type".into(), Value::String(value.type_name().to_owned()));
            object.insert("kind".into(), Value::String(value.kind().as_str().to_owned()));
            object.insert("members".into(), Value::Array(members));
            description = Value::Object(object);
            Ok(())
        })?;
        Ok(description)
    }

    /// Assigns `value` at `path`. The path must pass a lock, atomic or signal; see
    /// [`Reflect::write_shared`].
    pub fn set(&self, path: &str, value: &Value) -> Result<(), Error> {
        let (root, segments) = self.resolve(path)?;
        root.write_shared(&segments, &mut |target| target.assign(value))
    }
}
