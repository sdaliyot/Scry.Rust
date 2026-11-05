use std::rc::Rc;

use serde_json::{Map, Value, json};

use crate::__private::FieldRef;
use crate::path::parse;
use crate::reflect::short_type_name;
use crate::{Error, Member, Reflect, Segment, Visit, VisitMut};

/// Named roots that paths start from.
///
/// Deliberately not `Send`: roots may be UI-thread values such as Dioxus signals, so a registry
/// lives on the thread that owns its roots, and other threads reach it by sending it requests.
#[derive(Default)]
pub struct Registry {
    /// Shared only while a script runs, whose handles keep their own reference; see [`Registry::share`].
    roots: Rc<Vec<(String, Root)>>,
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
        registry.roots_mut().sort_by(|(a, _), (b, _)| a.cmp(b));
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
        let roots = self.roots_mut();
        roots.retain(|(existing, _)| *existing != name);
        roots.push((name, root));
        self
    }

    fn roots_mut(&mut self) -> &mut Vec<(String, Root)> {
        Rc::get_mut(&mut self.roots).expect("roots are added before the registry is shared")
    }

    /// A second handle on the same roots, for the handles a script holds. Dropped with the script,
    /// so roots can still be added afterwards.
    #[cfg_attr(not(feature = "script"), allow(dead_code))]
    pub(crate) fn share(&self) -> Registry {
        Registry { roots: Rc::clone(&self.roots) }
    }

    pub fn roots(&self) -> Vec<Member> {
        self.roots.iter().map(|(name, root)| Member::new(name.clone(), root.get().type_name())).collect()
    }

    fn resolve(&self, path: &str) -> Result<(&dyn Reflect, Vec<Segment>), Error> {
        let (root, segments) = parse(path)?;
        Ok((self.find(&root)?.1.get(), segments))
    }

    /// The full name of the root `root` refers to.
    #[cfg_attr(not(feature = "script"), allow(dead_code))]
    pub(crate) fn canonical(&self, root: &str) -> Result<&str, Error> {
        Ok(&self.find(root)?.0)
    }

    /// Resolves `segments` below `root` and calls `visit` on the result.
    #[cfg_attr(not(feature = "script"), allow(dead_code))]
    pub(crate) fn read_at(&self, root: &str, segments: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        self.find(root)?.1.get().read(segments, visit)
    }

    /// Assigns `value` to what `segments` below `root` resolve to.
    #[cfg_attr(not(feature = "script"), allow(dead_code))]
    pub(crate) fn set_at(&self, root: &str, segments: &[Segment], value: &Value) -> Result<(), Error> {
        self.find(root)?.1.get().write_shared(segments, &mut |target| target.assign(value))
    }

    /// Resolves `segments` below `root` for writing and calls `visit` on the result.
    pub(crate) fn write_shared_at(&self, root: &str, segments: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.find(root)?.1.get().write_shared(segments, visit)
    }

    /// An exact root name, or the unique one ending in `::name`.
    fn find(&self, root: &str) -> Result<&(String, Root), Error> {
        if let Some(entry) = self.roots.iter().find(|(name, _)| name == root) {
            return Ok(entry);
        }
        let suffix = format!("::{root}");
        let matches: Vec<&(String, Root)> = self.roots.iter().filter(|(name, _)| name.ends_with(&suffix)).collect();
        match matches.as_slice() {
            [entry] => Ok(entry),
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
            let members: Vec<Value> = value
                .members()
                .into_iter()
                .map(|member| json!({ "name": member.name, "type": short_type_name(member.type_name) }))
                .collect();
            let mut object = Map::new();
            object.insert("path".into(), Value::String(path.trim().to_owned()));
            object.insert("type".into(), Value::String(short_type_name(value.type_name())));
            object.insert("fullType".into(), Value::String(value.type_name().to_owned()));
            object.insert("kind".into(), Value::String(value.kind().as_str().to_owned()));
            object.insert("members".into(), Value::Array(members));
            let methods = crate::call::methods_of(value);
            if !methods.is_empty() {
                object.insert("methods".into(), json!(methods));
            }
            description = Value::Object(object);
            Ok(())
        })?;
        Ok(description)
    }

    /// Assigns `value` at `path`. The path must pass a lock, atomic or signal; see
    /// [`Reflect::write_shared`].
    pub fn set(&self, path: &str, value: &Value) -> Result<(), Error> {
        let (root, segments) = parse(path)?;
        self.set_at(&root, &segments, value)
    }
}
