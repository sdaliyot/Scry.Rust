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
//! the script finishes without an error.
//!
//! Nodes have methods that never clash with field names, because fields are reached as properties
//! and methods are called: `get()`, `get(depth)`, `set(value)`, `members()`, `children()`,
//! `len()`, `type_name()`, `kind()`, `path()`. When the value's type has a method of its own with
//! the same name and argument count (`Cache::get(key)`, `Inventory::len()`), that one runs
//! instead; `get(node.path())` still takes a snapshot.
//!
//! The app's own methods are called with method syntax on a node - `APP.inventory.total_value()`,
//! `board.toggle(2)` - and its free functions by name when the name is unique (`ratio(3, 4)`).
//! `invoke("util::ratio", [3, 4])` and `node.invoke("len", [])` reach any of them, including ones
//! whose names clash with the methods above (`call` is Rhai's own, for function pointers). Arguments are passed as JSON, so a node argument passes a
//! copy of its value; results come back as plain values.
//!
//! A script runs on the registry's thread - the UI thread in a Dioxus app - so it is limited to
//! [`TIME_LIMIT`] and to bounded string, array and map sizes, and it cannot load modules.

use std::any::TypeId;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rhai::module_resolvers::DummyModuleResolver;
use rhai::{Array, Dynamic, Engine, EvalAltResult, INT, ImmutableString, Map, Position, Scope};
use serde_json::{Number, Value};

use crate::command::RESULT_DEPTH;
use crate::{Error, Kind, Reflect, Registry, Segment, call};

/// How long a script may run before it is stopped.
pub const TIME_LIMIT: Duration = Duration::from_secs(5);

/// How deep a node is copied when it is assigned to another member or passed to a function.
const ASSIGN_DEPTH: usize = 64;

/// Names scripts already use; app functions of these names are reached through `call` only.
const RESERVED: &[&str] = &[
    "get",
    "set",
    "members",
    "children",
    "len",
    "type_name",
    "kind",
    "path",
    "to_string",
    "to_debug",
    "call",
    "invoke",
    "root",
    "roots",
    "print",
    "debug",
    "type_of",
    "eval",
];

/// Runs `source` and returns its last value as JSON, nodes as snapshots `depth` levels deep.
/// What the script prints is appended to `output`, also when it fails.
pub(crate) fn evaluate(
    registry: &Registry,
    source: &str,
    depth: usize,
    output: &mut Vec<String>,
) -> Result<Value, Error> {
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

    // A type's own method of the same name and argument count wins over each of these.
    engine.register_fn("get", |node: Node| {
        node.own_or("get", &[], |node| node.with(|value| to_dynamic(&value.snapshot(crate::command::DEFAULT_DEPTH))))
    });
    engine.register_fn("get", |node: Node, arg: Dynamic| {
        node.own_or("get", std::slice::from_ref(&arg), |node| {
            let depth = arg
                .as_int()
                .map_err(|_| Error::Script(format!("get(depth) takes a number, not {}", arg.type_name())))?;
            node.with(|value| to_dynamic(&value.snapshot(depth.max(0) as usize)))
        })
    });
    engine.register_fn("set", |node: Node, value: Dynamic| {
        node.own_or("set", std::slice::from_ref(&value), |node| node.assign(&value).map(|()| Dynamic::UNIT))
    });
    engine.register_fn("members", |node: Node| {
        node.own_or("members", &[], |node| {
            let members = node.with(|value| value.members())?;
            Ok(members.into_iter().map(|member| Dynamic::from(member.name)).collect::<Array>().into())
        })
    });
    engine.register_fn("children", |node: Node| {
        node.own_or("children", &[], |node| node.children().map(Dynamic::from_array))
    });
    engine.register_fn("len", |node: Node| {
        node.own_or("len", &[], |node| node.with(|value| (value.member_count() as INT).into()))
    });
    engine.register_fn("type_name", |node: Node| {
        node.own_or("type_name", &[], |node| {
            node.with(|value| crate::reflect::short_type_name(value.type_name()).into())
        })
    });
    engine.register_fn("kind", |node: Node| {
        node.own_or("kind", &[], |node| node.with(|value| value.kind().as_str().into()))
    });
    engine.register_fn("path", |node: Node| node.own_or("path", &[], |node| Ok(node.path().into())));
    engine.register_fn("to_string", |node: Node| node.to_string());
    engine.register_fn("to_debug", |node: Node| node.to_string());

    engine.register_fn("invoke", |node: Node, method: &str, args: Array| node.call(method, &args).map_err(raise));
    engine.register_fn("invoke", |node: Node, method: &str| node.call(method, &[]).map_err(raise));
    let shared = Rc::clone(registry);
    engine.register_fn("invoke", move |target: &str, args: Array| {
        let args = json_args(&args).map_err(raise)?;
        shared.call(target, &args, RESULT_DEPTH).map(|value| to_dynamic(&value)).map_err(raise)
    });
    let shared = Rc::clone(registry);
    engine.register_fn("invoke", move |target: &str| {
        shared.call(target, &[], RESULT_DEPTH).map(|value| to_dynamic(&value)).map_err(raise)
    });
    for (name, arity) in call::method_names() {
        if RESERVED.contains(&name) {
            continue;
        }
        let mut types = vec![TypeId::of::<Node>()];
        types.extend(std::iter::repeat_n(TypeId::of::<Dynamic>(), arity));
        engine.register_raw_fn(name, types, move |_, args: &mut [&mut Dynamic]| {
            let node = args[0].clone().cast::<Node>();
            let args: Vec<Dynamic> = args[1..].iter().map(|arg| (**arg).clone()).collect();
            node.call(name, &args).map_err(raise)
        });
    }
    for (name, arity) in call::free_names() {
        if RESERVED.contains(&name) {
            continue;
        }
        engine.register_raw_fn(name, vec![TypeId::of::<Dynamic>(); arity], move |_, args: &mut [&mut Dynamic]| {
            let args: Vec<Dynamic> = args.iter().map(|arg| (**arg).clone()).collect();
            let args = json_args(&args).map_err(raise)?;
            call::call_free(name, &args, RESULT_DEPTH).map(|value| to_dynamic(&value)).map_err(raise)
        });
    }

    // Hooks, without `hook_wait`: a script runs on the registry's thread - a Dioxus app's UI
    // thread - so waiting there would stop the calls it waits for. `scry-rs hook wait` waits elsewhere.
    let hook = |line: String| -> Result<Dynamic, Box<EvalAltResult>> {
        match crate::hooks::command(&line) {
            Some(result) => result.map(|value| to_dynamic(&value)).map_err(raise),
            None => Err(raise(Error::BadCommand(line))),
        }
    };
    engine.register_fn("hook_add", move |path: &str| hook(format!("hook add {path}")));
    engine.register_fn("hook_add", move |path: &str, options: &str| hook(format!("hook add {path} {options}")));
    engine.register_fn("hook_read", move |id: INT| hook(format!("hook read {id}")));
    engine.register_fn("hook_read", move |id: INT, cursor: INT| hook(format!("hook read {id} {cursor}")));
    engine.register_fn("hook_remove", move |id: INT| hook(format!("hook remove {id}")));

    let shared = Rc::clone(registry);
    engine.register_fn("roots", move || -> Array { shared.roots().into_iter().map(|root| root.name.into()).collect() });
    let shared = Rc::clone(registry);
    engine.register_fn("root", move |name: &str| Node::root(&shared, name).map_err(raise));
    let shared = Rc::clone(registry);
    engine.register_fn("get", move |path: &str| {
        shared.get(path, crate::command::DEFAULT_DEPTH).map(|value| to_dynamic(&value)).map_err(raise)
    });
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

    /// The type's own method `name` when it has one taking `args`, else `fallback` - one of the
    /// methods every node has.
    fn own_or(
        &self,
        name: &str,
        args: &[Dynamic],
        fallback: impl FnOnce(&Node) -> Result<Dynamic, Error>,
    ) -> Result<Dynamic, Box<EvalAltResult>> {
        match self.call(name, args) {
            Err(Error::NoSuchMethod { .. } | Error::ArgumentCount { .. }) => fallback(self),
            result => result,
        }
        .map_err(raise)
    }

    fn call(&self, method: &str, args: &[Dynamic]) -> Result<Dynamic, Error> {
        let args = json_args(args)?;
        self.registry
            .call_method_at(&self.root, &self.segments, method, &args, RESULT_DEPTH)
            .map(|value| to_dynamic(&value))
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
            Ok(type_name) => write!(f, "{}: {}", self.path(), crate::reflect::short_type_name(type_name)),
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

/// How many operations a change script may take. It runs on the app's own thread, inside the hooked
/// call, so it has to be short.
const CHANGE_OPERATIONS: u64 = 100_000;

thread_local! {
    /// Compiled change scripts, per thread: an engine is not `Send`, and a hooked function runs on
    /// whatever thread calls it.
    static CHANGE_SCRIPTS: RefCell<std::collections::HashMap<String, Rc<(Engine, rhai::AST)>>> = RefCell::default();
}

fn change_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_module_resolver(DummyModuleResolver::new());
    engine.set_max_operations(CHANGE_OPERATIONS);
    engine.set_max_string_size(1 << 20);
    engine.set_max_array_size(100_000);
    engine.set_max_map_size(100_000);
    engine
}

/// Whether `script` compiles: a `hook before`/`hook after` script, or a `wait` condition.
pub(crate) fn check(script: &str) -> Result<(), Error> {
    change_engine().compile(script).map(|_| ()).map_err(|error| Error::Script(error.to_string()))
}

/// Runs a change script with `input`'s fields as its variables, and returns what it evaluated to.
pub(crate) fn change(script: &str, input: serde_json::Map<String, Value>) -> Result<Value, Error> {
    let compiled = CHANGE_SCRIPTS.with(|scripts| -> Result<Rc<(Engine, rhai::AST)>, Error> {
        let mut scripts = scripts.borrow_mut();
        if let Some(compiled) = scripts.get(script) {
            return Ok(Rc::clone(compiled));
        }
        let engine = change_engine();
        let ast = engine.compile(script).map_err(|error| Error::Script(error.to_string()))?;
        if scripts.len() >= 64 {
            scripts.clear();
        }
        let compiled = Rc::new((engine, ast));
        scripts.insert(script.to_owned(), Rc::clone(&compiled));
        Ok(compiled)
    })?;
    let (engine, ast) = &*compiled;
    let mut scope = Scope::new();
    for (name, value) in &input {
        scope.push(name.clone(), to_dynamic(value));
    }
    let result = engine.eval_ast_with_scope::<Dynamic>(&mut scope, ast).map_err(|error| failure(&error))?;
    to_json(&result, ASSIGN_DEPTH)
}

/// An engine for `hook wait` predicates: no access to the app, only to the recorded call.
pub(crate) fn predicate_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_module_resolver(DummyModuleResolver::new());
    engine.set_max_operations(1_000_000);
    engine
}

/// Whether a recorded call satisfies a predicate. Its fields are variables: `args`, `result`,
/// `panic`, `cancelled`, `instance` (the captured `self`), `seq`, `duration_micros`, `thread`, and
/// `call` for the whole record.
pub(crate) fn predicate(engine: &Engine, ast: &rhai::AST, call: &Value) -> Result<bool, Error> {
    let mut scope = Scope::new();
    for (variable, field) in [
        ("args", "args"),
        ("result", "result"),
        ("panic", "panic"),
        ("cancelled", "cancelled"),
        ("instance", "self"),
        ("seq", "seq"),
        ("duration_micros", "durationMicros"),
        ("thread", "thread"),
    ] {
        scope.push(variable, to_dynamic(&call[field]));
    }
    scope.push("call", to_dynamic(call));
    let value = engine.eval_ast_with_scope::<Dynamic>(&mut scope, ast).map_err(|error| failure(&error))?;
    value.as_bool().map_err(|_| Error::Script(format!("the predicate gave {}, not a bool", value.type_name())))
}

fn json_args(args: &[Dynamic]) -> Result<Vec<serde_json::Value>, Error> {
    args.iter().map(|arg| to_json(arg, ASSIGN_DEPTH)).collect()
}

fn raise(error: Error) -> Box<EvalAltResult> {
    EvalAltResult::ErrorRuntime(Dynamic::from(error), Position::NONE).into()
}

/// The registry error a native function raised, keeping its code, or the script's own failure.
fn failure(error: &EvalAltResult) -> Error {
    match error.unwrap_inner() {
        EvalAltResult::ErrorRuntime(value, _) if value.is::<Error>() => value.clone().cast::<Error>(),
        EvalAltResult::ErrorTerminated(..) => Error::ScriptLimit(format!("ran longer than {} s", TIME_LIMIT.as_secs())),
        EvalAltResult::ErrorTooManyOperations(_)
        | EvalAltResult::ErrorDataTooLarge(..)
        | EvalAltResult::ErrorStackOverflow(_) => Error::ScriptLimit(error.to_string()),
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
        Value::Object(fields) => {
            Dynamic::from_map(fields.iter().map(|(name, value)| (name.into(), to_dynamic(value))).collect())
        }
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
