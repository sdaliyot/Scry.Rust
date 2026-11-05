//! Signals that live inside components, exposed under the `ui` root.
//!
//! `let uptime = use_signal(|| 0)` has no name the registry could look up: the value belongs to
//! one mounted instance of a component, and there may be many instances or none. The generator
//! wraps each such hook call so it also calls [`track`], which records a copy of the handle under
//! the component and binding name for as long as that instance stays mounted. Paths then read
//! `ui.App.uptime`, or `ui.TaskRow[2].done` when a component is mounted more than once.
//!
//! Everything here is on the UI thread, like the signals themselves.

use std::any::type_name;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use dioxus_core::{current_scope_id, spawn, use_hook};
use futures_channel::mpsc::UnboundedReceiver;
use futures_util::StreamExt;
use serde_json::{Map, Value};

use crate::__private::truncated;
use crate::endpoint::Job;
use crate::{Error, Kind, Member, Reflect, Segment, Visit, VisitMut};

struct Hook {
    id: u64,
    scope: usize,
    component: &'static str,
    name: &'static str,
    value: Box<dyn Reflect>,
}

thread_local! {
    static HOOKS: RefCell<Vec<Hook>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
}

/// Removes its hook when the component instance that owns it is dropped.
struct Registration(u64);

impl Drop for Registration {
    fn drop(&mut self) {
        let id = self.0;
        let _ = HOOKS.try_with(|hooks| {
            if let Ok(mut hooks) = hooks.try_borrow_mut() {
                hooks.retain(|hook| hook.id != id);
            }
        });
    }
}

/// Records a hook's handle for the current component instance. Called by generated code right
/// after the hook it wraps, so it runs on every render in the same hook position; the handle is
/// copied once, on the first render.
pub fn track(component: &'static str, name: &'static str, value: impl FnOnce() -> Box<dyn Reflect>) {
    use_hook(|| {
        let id = NEXT_ID.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        let scope = current_scope_id().0;
        HOOKS.with(|hooks| hooks.borrow_mut().push(Hook { id, scope, component, name, value: value() }));
        Rc::new(Registration(id))
    });
}

/// Jobs from the endpoint, waiting for the root component to start pumping them.
static JOBS: std::sync::Mutex<Option<UnboundedReceiver<Job>>> = std::sync::Mutex::new(None);

/// Starts the endpoint for a Dioxus app. Called at the top of `main`; the registry itself runs on
/// the UI thread, in the task [`use_endpoint`] starts, because that is where signals live.
pub fn start_endpoint() {
    crate::run_async::capture_runtime();
    let (sender, receiver) = futures_channel::mpsc::unbounded::<Job>();
    if let Ok(mut jobs) = JOBS.lock() {
        *jobs = Some(receiver);
    }
    if let Err(error) = crate::endpoint::listen(move |job| sender.unbounded_send(job).is_ok()) {
        eprintln!("scry: could not start the endpoint: {error}");
    }
}

/// Runs the registry on the Dioxus runtime. Called at the top of the root component.
pub fn use_endpoint() {
    use_hook(|| {
        crate::run_async::capture_runtime();
        let jobs = JOBS.lock().ok().and_then(|mut jobs| jobs.take());
        if let Some(mut jobs) = jobs {
            spawn(async move {
                let registry = crate::Registry::discovered();
                while let Some((command, reply)) = jobs.next().await {
                    let _ = reply.send(crate::execute(&registry, &command).to_string());
                }
            });
        }
    });
}

/// The `ui` root.
pub struct Ui;

/// One mounted component instance: its scope's hooks, in creation order.
struct Instance<'a> {
    component: &'static str,
    hooks: Vec<&'a Hook>,
}

/// All mounted instances of one component.
struct Component<'a> {
    instances: Vec<Instance<'a>>,
}

fn with_tree<R>(f: impl FnOnce(&BTreeMap<&'static str, Component<'_>>) -> R) -> R {
    HOOKS.with(|hooks| {
        let hooks = hooks.borrow();
        let mut scopes: BTreeMap<usize, Vec<&Hook>> = BTreeMap::new();
        for hook in hooks.iter() {
            scopes.entry(hook.scope).or_default().push(hook);
        }
        let mut tree: BTreeMap<&'static str, Component<'_>> = BTreeMap::new();
        for (_, hooks) in scopes {
            // Hooks called from a custom `use_*` hook carry that function's name; the instance is
            // named after the component that called it.
            let component = hooks
                .iter()
                .map(|hook| hook.component)
                .find(|name| !name.starts_with("use_"))
                .unwrap_or(hooks[0].component);
            tree.entry(component)
                .or_insert_with(|| Component { instances: Vec::new() })
                .instances
                .push(Instance { component, hooks });
        }
        f(&tree)
    })
}

fn not_writable_here(type_name: &'static str) -> Error {
    Error::needs_interior_mutability(type_name)
}

impl Reflect for Instance<'_> {
    fn type_name(&self) -> &'static str {
        self.component
    }

    fn kind(&self) -> Kind {
        Kind::Struct
    }

    fn members(&self) -> Vec<Member> {
        self.hooks.iter().map(|hook| Member::new(hook.name, hook.value.type_name())).collect()
    }

    fn snapshot(&self, depth: usize) -> Value {
        if depth == 0 {
            return truncated(self.component);
        }
        let mut object = Map::new();
        for hook in &self.hooks {
            object.insert(hook.name.to_owned(), hook.value.snapshot(depth - 1));
        }
        Value::Object(object)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else {
            return visit(self);
        };
        self.hook(segment)?.value.read(rest, visit)
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else {
            return Err(not_writable_here(self.component));
        };
        self.hook(segment)?.value.write_shared(rest, visit)
    }
}

impl Instance<'_> {
    fn hook(&self, segment: &Segment) -> Result<&Hook, Error> {
        self.hooks
            .iter()
            .copied()
            .find(|hook| segment.field() == Some(hook.name))
            .ok_or_else(|| crate::__private::no_member(self.component, segment))
    }
}

/// A component mounted once reads as that instance; mounted several times, as a list of them.
impl Reflect for Component<'_> {
    fn type_name(&self) -> &'static str {
        match self.instances.as_slice() {
            [single] => single.component,
            _ => type_name::<Self>(),
        }
    }

    fn kind(&self) -> Kind {
        match self.instances.as_slice() {
            [single] => single.kind(),
            _ => Kind::Sequence,
        }
    }

    fn members(&self) -> Vec<Member> {
        match self.instances.as_slice() {
            [single] => single.members(),
            many => many
                .iter()
                .enumerate()
                .map(|(index, instance)| Member::new(format!("[{index}]"), instance.component))
                .collect(),
        }
    }

    fn snapshot(&self, depth: usize) -> Value {
        match self.instances.as_slice() {
            [single] => single.snapshot(depth),
            _ if depth == 0 => truncated("instances"),
            many => Value::Array(many.iter().map(|instance| instance.snapshot(depth - 1)).collect()),
        }
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        match self.instances.as_slice() {
            [single] => single.read(path, visit),
            many => match path.split_first() {
                None => visit(self),
                Some((segment, rest)) => instance(many, segment)?.read(rest, visit),
            },
        }
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        match self.instances.as_slice() {
            [single] => single.write_shared(path, visit),
            many => match path.split_first() {
                None => Err(not_writable_here(self.type_name())),
                Some((segment, rest)) => instance(many, segment)?.write_shared(rest, visit),
            },
        }
    }
}

fn instance<'a, 'b>(instances: &'a [Instance<'b>], segment: &Segment) -> Result<&'a Instance<'b>, Error> {
    let index = segment.index().ok_or_else(|| {
        Error::BadPath(format!("this component is mounted {} times; pick one with [index]", instances.len()))
    })?;
    instances.get(index).ok_or(Error::IndexOutOfRange { len: instances.len(), index })
}

impl Reflect for Ui {
    fn type_name(&self) -> &'static str {
        "ui"
    }

    fn kind(&self) -> Kind {
        Kind::Struct
    }

    fn members(&self) -> Vec<Member> {
        with_tree(|tree| tree.iter().map(|(name, component)| Member::new(*name, component.type_name())).collect())
    }

    fn snapshot(&self, depth: usize) -> Value {
        if depth == 0 {
            return truncated("ui");
        }
        with_tree(|tree| {
            Value::Object(
                tree.iter().map(|(name, component)| ((*name).to_owned(), component.snapshot(depth - 1))).collect(),
            )
        })
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else {
            return visit(self);
        };
        with_tree(|tree| component(tree, segment)?.read(rest, visit))
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else {
            return Err(not_writable_here("ui"));
        };
        with_tree(|tree| component(tree, segment)?.write_shared(rest, visit))
    }
}

fn component<'a, 'b>(
    tree: &'a BTreeMap<&'static str, Component<'b>>,
    segment: &Segment,
) -> Result<&'a Component<'b>, Error> {
    segment.field().and_then(|name| tree.get(name)).ok_or_else(|| crate::__private::no_member("ui", segment))
}
