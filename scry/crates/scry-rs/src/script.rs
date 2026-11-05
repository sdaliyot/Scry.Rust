//! The `eval` command: [Rhai](https://rhai.rs) scripts over the registry.
//!
//! Every root is a script variable, by its short name when that is unique (`APP` for
//! `console_demo::APP`) and always through `root("console_demo::APP")`. Members are reached the
//! way Rust code reaches them - `APP.rules[0].percent` - and assigning to one writes it through
//! [`Registry::set`]'s rules, so a write still needs a lock, atomic or signal on its path:
//!
//! ```text
//! APP.stats.ticks += 1;
//! for rule in APP.rules { if rule.percent > 40 { print(rule.get()); } }
//! APP.rules.len()
//! ```
//!
//! A member with members of its own reads as a node: a live handle that re-reads the app on every
//! access. Anything else - numbers, strings, `None`, unit enum variants, opaque values - reads as a
//! plain copy. So does a root without members (`DISCOUNTS_ENABLED`, `FILTER`): it is read when the
//! script starts, and if the script assigns it (`FILTER = "Done"`) the value is written back when
//! the script finishes without an error. Nodes have methods that never clash with field names, because fields are reached
//! as properties and methods are called: `get()`, `get(depth)`, `set(value)`, `members()`,
//! `children()`, `len()`, `type_name()`, `kind()`, `path()`. Global `roots()`, `root(name)`,
//! `get(path)`, `get(path, depth)` and `set(path, value)` mirror the other commands.
//!
//! A script runs on the registry's thread - the UI thread in a Dioxus app - so it is limited to
//! [`TIME_LIMIT`] and to bounded string, array and map sizes, and it cannot load modules.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rhai::module_resolvers::DummyModuleResolver;
use rhai::{Array, Dynamic, Engine, EvalAltResult, INT, ImmutableString, Map, Position, Scope};
use serde_json::{Number, Value};

use crate::{Error, Kind, Reflect, Registry, Segment};

/// How long a script may run before it is stopped.
pub const TIME_LIMIT: Duration = Duration::from_secs(5);

/// How deep a node is copied when it is assigned to another member.
const ASSIGN_DEPTH: usize = 64;

/// Runs `source` and returns its last value as JSON, nodes as snapshots `depth` levels deep.
/// What the script prints is appended to `output`, also when it fails.
pub(crate) fn evaluate(registry: &Registry, source: &str, depth: usize, output: &mut Vec<String>) -> Result<Value, Error> {
    let registry = Rc::new(registry.share());
    let printed = Rc::new(RefCell::new(Vec::new()));
    let engine = engine(&registry, &printed);

    let mut scope = Scope::new();
    let mut roots = Vec::new();
    for (name, node) in variables(&registry) {
        let value = node.clone().value().unwrap_or_else(|_| Dynamic::from(node.clone()));
        scope.push(name.clone(), value.clone());
        roots.push((name, node, value));
    }
    let result = engine.eval_with_scope::<Dynamic>(&mut scope, source);
    output.append(&mut printed.borrow_mut());
    let result = result.map_err(|error| failure(&error))?;

    for (name, node, before) in roots {
        // The first variable of that name is the root's; a later `let` of the same name shadows it.
        let Some((_, _, after)) = scope.iter().find(|(variable, _, _)| *variable == name) else { continue };
        let unchanged = !after.is::<Node>() && to_json(&after, 0).ok() == to_json(&before, 0).ok();
        if !unchanged {
            node.assign(&after)?;
        }
    }
    to_json(&result, depth)
}

fn engine(registry: &Rc<Registry>, printed: &Rc<RefCell<Vec<String>>>) -> Engine {
    let mut engine = Engine::new();
    engine.set_module_resolver(DummyModuleResolver::new());
    engine.set_max_string_size(1 << 20);
    engine.set_max_array_size(100_000);
    engine.set_max_map_size(100_000);
    let started = Instant::now();
    engine.on_progress(move |operations| {
        (operations % 1024 == 0 && started.elapsed() > TIME_LIMIT).then_some(Dynamic::UNIT)
    });
    let sink = Rc::clone(printed);
    engine.on_print(move |text| sink.borrow_mut().push(text.to_owned()));
    let sink = Rc::clone(printed);
    engine.on_debug(move |text, _, position| sink.borrow_mut().push(format!("[{position}] {text}")));

    engine.register_type_with_name::<Node>("Node");
    engine.register_indexer_get(|node: &mut Node, member: ImmutableString| {
        node.child(Segment::Field(member.to_string())).value().map_err(raise)
    });
    engine.register_indexer_get(|node: &mut Node, index: INT| node.child(index_segment(index)?).value().map_err(raise));
    engine.register_indexer_set(|node: &mut Node, member: ImmutableString, value: Dynamic| {
        node.child(Segment::Field(member.to_string())).assign(&value).map_err(raise)
    });
    engine.register_indexer_set(|node: &mut Node, index: INT, value: Dynamic| {
        node.child(index_segment(index)?).assign(&value).map_err(raise)
    });
    engine.register_iterator::<Node>();

    engine.register_fn("get", |node: Node| node.snapshot(crate::command::DEFAULT_DEPTH));
    engine.register_fn("get", |node: Node, depth: INT| node.snapshot(depth.max(0) as usize));
    engine.register_fn("set", |node: Node, value: Dynamic| node.assign(&value).map_err(raise));
    engine.register_fn("members", |node: Node| -> Result<Array, Box<EvalAltResult>> {
        let members = node.with(|value| value.members()).map_err(raise)?;
        Ok(members.into_iter().map(|member| member.name.into()).collect())
    });
    engine.register_fn("children", |node: Node| node.children().map_err(raise));
    engine.register_fn("len", |node: Node| node.with(|value| value.member_count() as INT).map_err(raise));
    engine.register_fn("type_name", |node: Node| node.with(|value| value.type_name()).map_err(raise));
    engine.register_fn("kind", |node: Node| node.with(|value| value.kind().as_str()).map_err(raise));
    engine.register_fn("path", |node: Node| node.path());
    engine.register_fn("to_string", |node: Node| node.to_string());
    engine.register_fn("to_debug", |node: Node| node.to_string());

    let shared = Rc::clone(registry);
    engine.register_fn("roots", move || -> Array { shared.roots().into_iter().map(|root| root.name.into()).collect() });
    let shared = Rc::clone(registry);
    engine.register_fn("root", move |name: &str| Node::root(&shared, name).map_err(raise));
    let shared = Rc::clone(registry);
    engine.register_fn("get", move |path: &str| shared.get(path, crate::command::DEFAULT_DEPTH).map(|value| to_dynamic(&value)).map_err(raise));
    let shared = Rc::clone(registry);
    engine.register_fn("get", move |path: &str, depth: INT| {
        shared.get(path, depth.max(0) as usize).map(|value| to_dynamic(&value)).map_err(raise)
    });
    let shared = Rc::clone(registry);
    engine.register_fn("set", move |path: &str, value: Dynamic| {
        shared.set(path, &to_json(&value, ASSIGN_DEPTH).map_err(raise)?).map_err(raise)
    });
    engine
}

/// The roots as script variables: each by its full name when that is an identifier (`ui`), else by
/// its last `::` segment when no other root shares it.
fn variables(registry: &Rc<Registry>) -> Vec<(String, Node)> {
    let names: Vec<String> = registry.roots().into_iter().map(|root| root.name).collect();
    let short = |name: &str| name.rsplit("::").next().unwrap_or(name).to_owned();
    names
        .iter()
        .filter_map(|name| {
            let variable = if identifier(name) {
                name.clone()
            } else {
                let short = short(name);
                let unique = names.iter().filter(|other| other.rsplit("::").next() == Some(&short)).count() == 1;
                (unique && identifier(&short)).then_some(short)?
            };
            Some((variable, Node { registry: Rc::clone(registry), root: name.as_str().into(), segments: Vec::new() }))
        })
        .collect()
}

fn identifier(name: &str) -> bool {
    name.chars().next().is_some_and(|first| first.is_alphabetic() || first == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn index_segment(index: INT) -> Result<Segment, Box<EvalAltResult>> {
    usize::try_from(index).map(Segment::Index).map_err(|_| raise(Error::BadPath(format!("'{index}' is not an index"))))
}

/// A place in the app: a root and the path below it. Cheap to clone, and read afresh on each use.
#[derive(Clone)]
struct Node {
    registry: Rc<Registry>,
    root: Rc<str>,
    segments: Vec<Segment>,
}

impl Node {
    fn root(registry: &Rc<Registry>, name: &str) -> Result<Node, Error> {
        let root = registry.canonical(name)?.into();
        Ok(Node { registry: Rc::clone(registry), root, segments: Vec::new() })
    }

    fn child(&self, segment: Segment) -> Node {
        let mut segments = self.segments.clone();
        segments.push(segment);
        Node { registry: Rc::clone(&self.registry), root: Rc::clone(&self.root), segments }
    }

    fn path(&self) -> String {
        let mut path = self.root.to_string();
        for segment in &self.segments {
            match segment {
                Segment::Field(name) => {
                    path.push('.');
                    path.push_str(name);
                }
                Segment::Index(index) => path.push_str(&format!("[{index}]")),
            }
        }
        path
    }

    /// Calls `f` with the value this node resolves to, while any guard on the way is held.
    fn with<T>(&self, f: impl FnOnce(&dyn Reflect) -> T) -> Result<T, Error> {
        let mut f = Some(f);
        let mut result = None;
        self.registry.read_at(&self.root, &self.segments, &mut |value| {
            if let Some(f) = f.take() {
                result = Some(f(value));
            }
            Ok(())
        })?;
        result.ok_or_else(|| Error::Script(format!("{} was not reached", self.path())))
    }

    /// What a script sees here: a plain copy of a value without members, or this node.
    fn value(self) -> Result<Dynamic, Error> {
        let plain = self.with(|value| match value.kind() {
            Kind::Scalar | Kind::Opaque => Some(value.snapshot(0)),
            Kind::Enum => Some(value.snapshot(1)).filter(Value::is_string),
            Kind::Struct | Kind::Sequence | Kind::Tuple => None,
        })?;
        Ok(match plain {
            Some(value) => to_dynamic(&value),
            None => Dynamic::from(self),
        })
    }

    fn snapshot(self, depth: usize) -> Result<Dynamic, Box<EvalAltResult>> {
        self.with(|value| to_dynamic(&value.snapshot(depth))).map_err(raise)
    }

    fn children(&self) -> Result<Vec<Dynamic>, Error> {
        self.child_segments()?.into_iter().map(|segment| self.child(segment).value()).collect()
    }

    /// Every element of a sequence - not just the first 100 that `members` lists - or else every
    /// member.
    fn child_segments(&self) -> Result<Vec<Segment>, Error> {
        self.with(|value| match value.kind() {
            Kind::Sequence => (0..value.member_count()).map(Segment::Index).collect(),
            _ => value.members().into_iter().map(|member| member_segment(member.name)).collect(),
        })
    }

    fn assign(&self, value: &Dynamic) -> Result<(), Error> {
        if let Some(node) = value.clone().try_cast::<Node>() {
            // Rhai writes back the handles it read through on the way to a member it changed.
            if node.root == self.root && node.segments == self.segments {
                return Ok(());
            }
        }
        self.registry.set_at(&self.root, &self.segments, &to_json(value, ASSIGN_DEPTH)?)
    }
}

impl std::fmt::Display for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.with(|value| value.type_name()) {
            Ok(type_name) => write!(f, "{}: {type_name}", self.path()),
            Err(error) => write!(f, "{}: <{error}>", self.path()),
        }
    }
}

/// `for child in node` - the members' values. A member that cannot be read is skipped; use
/// `children()` to see why.
impl IntoIterator for Node {
    type Item = Dynamic;
    type IntoIter = std::vec::IntoIter<Dynamic>;

    fn into_iter(self) -> Self::IntoIter {
        let segments = self.child_segments().unwrap_or_default();
        segments.into_iter().filter_map(|segment| self.child(segment).value().ok()).collect::<Vec<_>>().into_iter()
    }
}

/// The segment for a member name: `[0]` is an index, anything else a field name or key.
fn member_segment(name: String) -> Segment {
    match name.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')).and_then(|index| index.parse().ok()) {
        Some(index) => Segment::Index(index),
        None => Segment::Field(name),
    }
}

fn raise(error: Error) -> Box<EvalAltResult> {
    EvalAltResult::ErrorRuntime(Dynamic::from(error), Position::NONE).into()
}

/// The registry error a native function raised, keeping its code, or the script's own failure.
fn failure(error: &EvalAltResult) -> Error {
    match error.unwrap_inner() {
        EvalAltResult::ErrorRuntime(value, _) if value.is::<Error>() => value.clone().cast::<Error>(),
        EvalAltResult::ErrorTerminated(..) => Error::ScriptLimit(format!("ran longer than {} s", TIME_LIMIT.as_secs())),
        EvalAltResult::ErrorTooManyOperations(_) | EvalAltResult::ErrorDataTooLarge(..) | EvalAltResult::ErrorStackOverflow(_) => {
            Error::ScriptLimit(error.to_string())
        }
        _ => Error::Script(error.to_string()),
    }
}

fn to_dynamic(value: &Value) -> Dynamic {
    match value {
        Value::Null => Dynamic::UNIT,
        Value::Bool(value) => (*value).into(),
        Value::Number(number) => match number.as_i64() {
            Some(value) => value.into(),
            None => number.as_f64().unwrap_or(f64::NAN).into(),
        },
        Value::String(value) => value.clone().into(),
        Value::Array(items) => Dynamic::from_array(items.iter().map(to_dynamic).collect()),
        Value::Object(fields) => Dynamic::from_map(fields.iter().map(|(name, value)| (name.into(), to_dynamic(value))).collect()),
    }
}

/// A script value as JSON. Nodes become snapshots `depth` levels deep; values JSON has no form
/// for (function pointers, ranges) become their type name.
fn to_json(value: &Dynamic, depth: usize) -> Result<Value, Error> {
    let value = value.flatten_clone();
    if value.is_unit() {
        return Ok(Value::Null);
    }
    if let Ok(value) = value.as_bool() {
        return Ok(value.into());
    }
    if let Ok(value) = value.as_int() {
        return Ok(value.into());
    }
    if let Ok(value) = value.as_float() {
        return Ok(Number::from_f64(value).map_or(Value::Null, Value::Number));
    }
    if let Ok(value) = value.as_char() {
        return Ok(value.to_string().into());
    }
    if value.is_string() {
        return Ok(value.into_string().unwrap_or_default().into());
    }
    if value.is_array() {
        let items = value.into_array().unwrap_or_default();
        return items.iter().map(|item| to_json(item, depth)).collect::<Result<_, _>>().map(Value::Array);
    }
    if value.is_map() {
        let fields = value.cast::<Map>();
        return fields
            .iter()
            .map(|(name, value)| Ok((name.to_string(), to_json(value, depth)?)))
            .collect::<Result<_, _>>()
            .map(Value::Object);
    }
    let type_name = value.type_name();
    match value.try_cast::<Node>() {
        Some(node) => node.with(|value| value.snapshot(depth)),
        None => Ok(Value::String(format!("<{type_name}>"))),
    }
}
