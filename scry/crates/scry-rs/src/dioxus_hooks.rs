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
use std::rc::{Rc, Weak};

use dioxus_core::{ErrorContext, Runtime, ScopeId, current_scope_id, spawn_forever, use_hook};
use futures_channel::mpsc::UnboundedReceiver;
use futures_util::StreamExt;
use serde_json::{Map, Value};

use crate::__private::truncated;
use crate::endpoint::Job;
use crate::{Error, Kind, Member, Reflect, Segment, Visit, VisitMut};

struct Hook {
    id: u64,
    /// The window it is in (see [`current_window`]); scope ids are only unique within one.
    window: Option<u64>,
    scope: usize,
    component: &'static str,
    name: &'static str,
    value: Box<dyn Reflect>,
}

thread_local! {
    static HOOKS: RefCell<Vec<Hook>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
    /// Error boundaries seen above tracked components, by window; see [`error_boundaries`].
    static BOUNDARIES: RefCell<Vec<(Option<u64>, ErrorContext)>> = const { RefCell::new(Vec::new()) };
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
        let window = current_window();
        remember_boundary(window);
        HOOKS.with(|hooks| hooks.borrow_mut().push(Hook { id, window, scope, component, name, value: value() }));
        Rc::new(Registration(id))
    });
}

/// Jobs from the endpoint, waiting for the root component to start pumping them.
static JOBS: std::sync::Mutex<Option<UnboundedReceiver<Job>>> = std::sync::Mutex::new(None);

/// Starts the endpoint for a Dioxus app. Called at the top of `main`; the registry itself runs on
/// the UI thread, in the task [`use_endpoint`] starts, because that is where signals live. The
/// descriptor is written only once that task runs, so the app isn't listed before it can answer.
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
        // The window that pumps the endpoint is window 0.
        current_window();
        let jobs = JOBS.lock().ok().and_then(|mut jobs| jobs.take());
        if let Some(mut jobs) = jobs {
            // At the root scope, not the component's: an error that reaches the root error boundary
            // unmounts the app's root component, and the endpoint has to outlive it to report it.
            spawn_forever(async move {
                let registry = crate::Registry::discovered();
                while let Some((command, reply)) = jobs.next().await {
                    let answer = answer(&registry, &command).await;
                    let _ = reply.send(answer.to_string());
                }
            });
            // Jobs are taken from here on, so clients may find the app now.
            crate::endpoint::publish();
        }
    });
}

/// Answers one command: in the window `window <n>` names, else in the endpoint's own.
async fn answer(registry: &crate::Registry, command: &str) -> Value {
    let reply = |result| crate::command::reply(result, Vec::new());
    let (runtime, command) = match in_window(command) {
        Ok(found) => found,
        Err(error) => return reply(Err(error)),
    };
    let within = |f: &mut dyn FnMut()| match &runtime {
        Some(runtime) => runtime.in_scope(ScopeId::APP, f),
        None => f(),
    };
    if command.trim() == "windows" {
        return reply(Ok(list_windows()));
    }
    #[cfg(feature = "dioxus-desktop")]
    if let Some(capture) = crate::dioxus_screenshot::parse(command) {
        return reply(match capture {
            Ok(capture) => screenshot(capture, &within).await,
            Err(error) => Err(error),
        });
    }
    match crate::dioxus_dom::script(command) {
        Some(Ok(script)) => {
            let mut eval = None;
            within(&mut || eval = Some(crate::dioxus_dom::start(&script)));
            match eval {
                Some(eval) => reply(crate::dioxus_dom::finish(eval).await),
                None => reply(Err(Error::BadCommand("the window is gone".into()))),
            }
        }
        Some(Err(error)) => reply(Err(error)),
        None => {
            let mut answer = Value::Null;
            within(&mut || answer = crate::execute(registry, command));
            answer
        }
    }
}

/// `screenshot [selector]`: finds the element's box through the page, then has the webview render it.
#[cfg(feature = "dioxus-desktop")]
async fn screenshot(
    capture: crate::dioxus_screenshot::Capture,
    within: &dyn Fn(&mut dyn FnMut()),
) -> Result<Value, Error> {
    use crate::dioxus_screenshot::{Capture, Clip};
    let gone = || Error::BadCommand("the window is gone".into());
    // The page too is captured by its box, the viewport: a capture without one waits for a frame
    // that a minimized window never draws.
    let script = match &capture {
        Capture::Page => crate::dioxus_dom::viewport_script(),
        Capture::Element(selector) => crate::dioxus_dom::rect_script(selector),
    };
    let mut eval = None;
    within(&mut || eval = Some(crate::dioxus_dom::start(&script)));
    let rect = crate::dioxus_dom::finish(eval.ok_or_else(gone)?).await?;
    let clip = Clip::from_rect(&rect)?;
    let element = matches!(capture, Capture::Element(_)).then(|| rect["element"].clone());
    let mut pending = None;
    within(&mut || pending = Some(crate::dioxus_screenshot::start(&clip)));
    pending.ok_or_else(gone)??.finish(element).await
}

// ---- Windows ----
//
// A Dioxus desktop app may open more windows (`window().new_window(...)`), each with a virtual
// dom and runtime of its own, all on the UI thread. Scry learns of a window when a component in it
// tracks a hook, and numbers windows in that order: 0 is the endpoint's own. `ui` shows the window
// a command runs in, and `window <n> <command>` runs any command - `get ui`, `set ui.route`,
// `click`, `eval` - in window n. `windows` lists them.

struct Window {
    id: u64,
    runtime: Weak<Runtime>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<Window>> = const { RefCell::new(Vec::new()) };
}

/// The window whose runtime is current, numbered on first sight; `None` outside any runtime.
fn current_window() -> Option<u64> {
    let runtime = Runtime::try_current()?;
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        windows.retain(|window| window.runtime.strong_count() > 0);
        if let Some(window) = windows.iter().find(|window| std::ptr::eq(window.runtime.as_ptr(), Rc::as_ptr(&runtime)))
        {
            return Some(window.id);
        }
        let id = NEXT_ID.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        windows.push(Window { id, runtime: Rc::downgrade(&runtime) });
        Some(id)
    })
}

/// The open windows' runtimes, in the order Scry learned of them.
fn open_windows() -> Vec<Rc<Runtime>> {
    WINDOWS.with(|windows| windows.borrow().iter().filter_map(|window| window.runtime.upgrade()).collect())
}

/// `window <n> <command>`: window n's runtime and the command; else no runtime and `command`.
fn in_window(command: &str) -> Result<(Option<Rc<Runtime>>, &str), Error> {
    let command = command.trim_start();
    let Some(rest) = command.strip_prefix("window").filter(|rest| rest.starts_with(char::is_whitespace)) else {
        return Ok((None, command));
    };
    let rest = rest.trim_start();
    let (index, command) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let windows = open_windows();
    let runtime =
        index.parse::<usize>().ok().and_then(|index| windows.get(index).cloned()).ok_or_else(|| {
            Error::BadCommand(format!("no window {index}; `windows` lists the {} open", windows.len()))
        })?;
    if command.trim().is_empty() {
        return Err(Error::BadCommand("expected `window <n> <command>`".into()));
    }
    Ok((Some(runtime), command))
}

/// `windows`: each window's number, its components with state, and its route.
fn list_windows() -> Value {
    let list = open_windows()
        .iter()
        .enumerate()
        .map(|(index, runtime)| {
            runtime.in_scope(ScopeId::APP, || {
                let mut window = Map::new();
                window.insert("window".into(), Value::from(index));
                let components: Vec<Value> =
                    with_tree(|tree| tree.keys().map(|name| Value::String((*name).to_owned())).collect());
                window.insert("components".into(), Value::Array(components));
                for (name, value) in extras() {
                    if name == "route" {
                        window.insert(name.into(), value.snapshot(1));
                    }
                }
                Value::Object(window)
            })
        })
        .collect();
    Value::Array(list)
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
        let window = current_window();
        let mut scopes: BTreeMap<usize, Vec<&Hook>> = BTreeMap::new();
        for hook in hooks.iter().filter(|hook| window.is_none() || hook.window == window) {
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
        let mut members: Vec<Member> =
            with_tree(|tree| tree.iter().map(|(name, component)| Member::new(*name, component.type_name())).collect());
        members.extend(extras().iter().map(|(name, value)| Member::new(*name, value.type_name())));
        members
    }

    fn snapshot(&self, depth: usize) -> Value {
        if depth == 0 {
            return truncated("ui");
        }
        #[allow(unused_mut)]
        let mut object: Map<String, Value> = with_tree(|tree| {
            tree.iter().map(|(name, component)| ((*name).to_owned(), component.snapshot(depth - 1))).collect()
        });
        for (name, value) in extras() {
            object.insert(name.to_owned(), value.snapshot(depth - 1));
        }
        Value::Object(object)
    }

    fn read(&self, path: &[Segment], visit: Visit<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else {
            return visit(self);
        };
        if let Some((_, value)) = extras().into_iter().find(|(name, _)| segment.field() == Some(*name)) {
            return value.read(rest, visit);
        }
        with_tree(|tree| component(tree, segment)?.read(rest, visit))
    }

    fn write(&mut self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        self.write_shared(path, visit)
    }

    fn write_shared(&self, path: &[Segment], visit: VisitMut<'_>) -> Result<(), Error> {
        let Some((segment, rest)) = path.split_first() else {
            return Err(not_writable_here("ui"));
        };
        #[cfg(feature = "dioxus-router")]
        if let Some(router) = crate::dioxus_router::router()
            && segment.field() == Some(crate::dioxus_router::ROUTE)
        {
            return match rest {
                [] => crate::dioxus_router::write(router, visit),
                _ => Err(Error::not_writable("set ui.route as a whole, to a route such as \"/\"")),
            };
        }
        if let Some(name) = segment.field().filter(|name| extras().iter().any(|(extra, _)| extra == name)) {
            return Err(Error::not_writable(format!("ui.{name} is read-only")));
        }
        with_tree(|tree| component(tree, segment)?.write_shared(rest, visit))
    }
}

/// The members of `ui` that are not components: the router's state, when the app has a router.
fn extras() -> Vec<(&'static str, Box<dyn Reflect>)> {
    let mut extras: Vec<(&'static str, Box<dyn Reflect>)> = Vec::new();
    if Runtime::try_current().is_some() {
        extras.push((ERRORS, Box::new(errors())));
    }
    #[cfg(feature = "dioxus-router")]
    if let Some(router) = crate::dioxus_router::router() {
        extras.extend(crate::dioxus_router::members(router));
    }
    extras
}

// ---- Errors ----
//
// An error a component returns or throws (a route that doesn't parse, an `onclick` that returns
// `Err`) goes to the nearest error boundary, which shows it instead of its children until it is
// cleared. `ui.errors` lists what the window's boundaries hold: the root one every virtual dom has,
// and the nearest one above each component Scry tracks. `ui::clear_errors` clears them, so they
// render their children again.

const ERRORS: &str = "errors";

/// The error boundaries Scry can see in the current window: the root one, and each boundary a
/// tracked component mounted under. One is remembered from then on while it shows an error - by
/// then the components under it have unmounted - or while a tracked component is still under it.
fn error_boundaries() -> Vec<ErrorContext> {
    let Some(runtime) = Runtime::try_current() else { return Vec::new() };
    let window = current_window();
    let scopes: Vec<usize> =
        HOOKS.with(|hooks| hooks.borrow().iter().filter(|hook| hook.window == window).map(|hook| hook.scope).collect());
    let mut live: Vec<ErrorContext> = Vec::new();
    for scope in std::iter::once(ScopeId::ROOT_ERROR_BOUNDARY.0).chain(scopes) {
        // A scope Scry tracks is mounted, but a lookup of one that isn't panics: don't let it.
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.consume_context::<ErrorContext>(ScopeId(scope))
        }));
        if let Ok(Some(boundary)) = found
            && !live.contains(&boundary)
        {
            live.push(boundary);
        }
    }
    BOUNDARIES.with(|seen| {
        let mut seen = seen.borrow_mut();
        seen.retain(|(in_window, boundary)| {
            *in_window != window || live.contains(boundary) || boundary.error().is_some()
        });
        for (_, boundary) in seen.iter().filter(|(in_window, _)| *in_window == window) {
            if !live.contains(boundary) {
                live.push(boundary.clone());
            }
        }
    });
    live
}

/// Remembers the boundary above the component being rendered; see [`error_boundaries`].
fn remember_boundary(window: Option<u64>) {
    let Some(boundary) = dioxus_core::try_consume_context::<ErrorContext>() else { return };
    BOUNDARIES.with(|seen| {
        let mut seen = seen.borrow_mut();
        if !seen.iter().any(|(in_window, known)| *in_window == window && *known == boundary) {
            seen.push((window, boundary));
        }
    });
}

/// `ui.errors`: the errors the window's boundaries are showing.
fn errors() -> Vec<String> {
    error_boundaries().iter().filter_map(ErrorContext::error).map(|error| error.to_string()).collect()
}

#[linkme::distributed_slice(crate::__private::FUNCTIONS)]
#[linkme(crate = linkme)]
static CLEAR_ERRORS: crate::call::Function = crate::call::Function {
    name: "clear_errors",
    path: "scry::ui::clear_errors",
    signature: "fn clear_errors() -> usize  // clears ui.errors, so their boundaries render again",
    receiver: crate::call::Receiver::None,
    self_type: None,
    trait_name: None,
    arity: 0,
    invoke: |_, _, _| {
        let boundaries: Vec<ErrorContext> =
            error_boundaries().into_iter().filter(|boundary| boundary.error().is_some()).collect();
        boundaries.iter().for_each(ErrorContext::clear_errors);
        Ok(Value::from(boundaries.len()))
    },
};

fn component<'a, 'b>(
    tree: &'a BTreeMap<&'static str, Component<'b>>,
    segment: &Segment,
) -> Result<&'a Component<'b>, Error> {
    segment.field().and_then(|name| tree.get(name)).ok_or_else(|| crate::__private::no_member("ui", segment))
}
