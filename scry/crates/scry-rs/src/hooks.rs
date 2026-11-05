//! Method hooks: recording the calls to one function while the app runs.
//!
//! `cargo scry` compiles a hook point into every hookable function - a [`HookSite`] and a few
//! lines around its body - so nothing is patched at run time. A site costs one relaxed atomic load
//! per call while no hook is on it. [`command`] turns recording on (`hook add`), and from then on
//! each call is captured: its arguments (and `self`, if asked) as snapshots when it starts, its
//! result or panic when it ends, its duration and thread. Captured calls go into a ring buffer that
//! `hook read` pages through and `hook wait` watches.
//!
//! Capturing never disturbs the hooked function:
//!
//! - it runs under `catch_unwind`, and a capture that fails is counted, not raised;
//! - locks are tried once and shown as busy rather than waited for, so a capture cannot stall the
//!   app's thread or deadlock on a lock that thread already holds;
//! - calls made while a capture is running on the same thread (a hooked function used by a `Debug`
//!   impl) are not recorded.
//!
//! The commands run without the registry: records are JSON, so the endpoint answers them on the
//! connection's thread, and a long `hook wait` never occupies the registry's (or a UI) thread.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::__private::FieldRef;
use crate::Error;

const DEFAULT_DEPTH: usize = 2;
const DEFAULT_CAPACITY: usize = 1000;
const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);
const DEFAULT_WAIT: Duration = Duration::from_secs(30);
/// `hook wait` is answered on the endpoint's connection thread, but a client still has to wait
/// for it; this keeps a typo in a timeout from holding a connection for hours.
const MAX_WAIT: Duration = Duration::from_secs(300);

/// One hookable function. The generator puts a `static` of these inside each function's body and
/// registers it in `__private::HOOK_SITES`.
pub struct HookSite {
    pub path: &'static str,
    pub signature: &'static str,
    active: AtomicBool,
    hook: Mutex<Option<Arc<Hook>>>,
}

impl HookSite {
    pub const fn new(path: &'static str, signature: &'static str) -> Self {
        Self { path, signature, active: AtomicBool::new(false), hook: Mutex::new(None) }
    }

    /// Called first thing in the hooked function. While the site is off this is one load; while
    /// it is on, `capture` is called to snapshot the arguments.
    #[inline]
    pub fn enter(&'static self, capture: impl FnOnce(&mut Capture)) -> Option<CallGuard> {
        if self.active.load(Ordering::Relaxed) { self.enter_active(capture) } else { None }
    }

    #[cold]
    fn enter_active(&'static self, capture: impl FnOnce(&mut Capture)) -> Option<CallGuard> {
        if CAPTURING.get() {
            return None;
        }
        let hook = self.hook.lock().ok()?.clone()?;
        if hook.expired() {
            hook.remove();
            return None;
        }
        let mut snapshot = Capture { depth: hook.depth, want_self: hook.capture_self, args: Map::new(), this: None };
        let captured = capturing(|| catch_unwind(AssertUnwindSafe(|| capture(&mut snapshot))).is_ok());
        if !captured {
            hook.capture_failures.fetch_add(1, Ordering::Relaxed);
        }
        Some(CallGuard {
            hook,
            start: Instant::now(),
            started_at: now_millis(),
            thread: thread_label(),
            args: Value::Object(snapshot.args),
            this: snapshot.this,
            finished: false,
        })
    }
}

/// Collects a call's arguments when it starts. Generated code calls [`Capture::arg`] for each
/// named parameter and [`Capture::this`] for the receiver.
pub struct Capture {
    depth: usize,
    want_self: bool,
    args: Map<String, Value>,
    this: Option<Value>,
}

impl Capture {
    pub fn arg(&mut self, name: &str, value: FieldRef<'_>) {
        self.args.insert(name.to_owned(), value.snapshot(self.depth));
    }

    pub fn this(&mut self, value: FieldRef<'_>) {
        if self.want_self {
            self.this = Some(value.snapshot(self.depth));
        }
    }
}

/// A call in progress. [`CallGuard::exit`] records its result; dropped without that, the call
/// panicked (or, for an `async fn`, its future was dropped before finishing).
pub struct CallGuard {
    hook: Arc<Hook>,
    start: Instant,
    started_at: f64,
    thread: String,
    args: Value,
    this: Option<Value>,
    finished: bool,
}

impl CallGuard {
    pub fn exit(mut self, result: FieldRef<'_>) {
        let depth = self.hook.depth;
        match capturing(|| catch_unwind(AssertUnwindSafe(|| result.snapshot(depth)))) {
            Ok(result) => self.finish("result", result),
            Err(_) => {
                self.hook.capture_failures.fetch_add(1, Ordering::Relaxed);
                self.finish("result", Value::String("<capture failed>".into()));
            }
        }
    }

    fn finish(&mut self, outcome: &str, value: Value) {
        self.finished = true;
        let mut record = json!({
            "startedAt": self.started_at,
            "durationMicros": self.start.elapsed().as_micros() as u64,
            "thread": self.thread,
            "args": std::mem::take(&mut self.args),
        });
        if let Some(this) = self.this.take() {
            record["self"] = this;
        }
        record[outcome] = value;
        self.hook.push(record);
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if thread::panicking() {
            let message = LAST_PANIC.take().unwrap_or_else(|| "a panic (its message was not captured)".into());
            self.finish("panic", Value::String(message));
        } else {
            self.finish("cancelled", Value::String("the future was dropped before it finished".into()));
        }
    }
}

thread_local! {
    /// Set while this thread captures, so calls made from inside a capture are not recorded.
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
    /// The message of the last panic on this thread, for the call guard that sees it unwind.
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn capturing<R>(f: impl FnOnce() -> R) -> R {
    CAPTURING.set(true);
    let result = crate::impls::without_waiting(f);
    CAPTURING.set(false);
    result
}

/// Records panic messages for [`CallGuard`]s, chained in front of whatever hook the app has.
/// Installed the first time a hook is added.
fn install_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = info.payload_as_str().unwrap_or("a panic without a message");
            let location = info.location().map(|location| format!(" at {location}")).unwrap_or_default();
            let _ = LAST_PANIC.try_with(|last| *last.borrow_mut() = Some(format!("{message}{location}")));
            previous(info);
        }));
    });
}

fn now_millis() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |time| time.as_secs_f64() * 1000.0)
}

fn thread_label() -> String {
    let current = thread::current();
    let id = format!("{:?}", current.id());
    let id = id.trim_start_matches("ThreadId(").trim_end_matches(')');
    match current.name() {
        Some(name) => format!("{name} ({id})"),
        None => format!("thread {id}"),
    }
}

/// An active hook on one site.
struct Hook {
    id: u64,
    site: &'static HookSite,
    depth: usize,
    capture_self: bool,
    capacity: usize,
    ttl: Duration,
    calls: Mutex<Calls>,
    arrived: Condvar,
    capture_failures: AtomicU64,
    last_used: Mutex<Instant>,
}

#[derive(Default)]
struct Calls {
    records: VecDeque<Value>,
    /// The `seq` the next finished call gets.
    next: u64,
    dropped: u64,
}

impl Hook {
    fn push(&self, mut record: Value) {
        let Ok(mut calls) = self.calls.lock() else { return };
        record["seq"] = json!(calls.next);
        calls.next += 1;
        calls.records.push_back(record);
        while calls.records.len() > self.capacity {
            calls.records.pop_front();
            calls.dropped += 1;
        }
        drop(calls);
        self.arrived.notify_all();
    }

    fn touch(&self) {
        if let Ok(mut last) = self.last_used.lock() {
            *last = Instant::now();
        }
    }

    fn expired(&self) -> bool {
        self.last_used.lock().is_ok_and(|last| last.elapsed() > self.ttl)
    }

    fn remove(&self) {
        self.site.active.store(false, Ordering::Relaxed);
        if let Ok(mut hook) = self.site.hook.lock() {
            *hook = None;
        }
        if let Ok(mut hooks) = HOOKS.lock() {
            hooks.retain(|hook| hook.id != self.id);
        }
        self.arrived.notify_all();
    }

    fn summary(&self) -> Value {
        let (next, dropped) = self.calls.lock().map(|calls| (calls.next, calls.dropped)).unwrap_or_default();
        json!({
            "hook": self.id,
            "path": self.site.path,
            "calls": next,
            "droppedCalls": dropped,
            "captureFailures": self.capture_failures.load(Ordering::Relaxed),
        })
    }

    /// Calls from `cursor` on, with what a reader needs to continue.
    fn read(&self, cursor: u64) -> Value {
        let Ok(calls) = self.calls.lock() else { return json!({}) };
        let oldest = calls.next - calls.records.len() as u64;
        let records: Vec<Value> = calls.records.iter().skip(cursor.saturating_sub(oldest) as usize).cloned().collect();
        json!({
            "calls": records,
            "cursor": calls.next,
            "truncated": cursor < oldest,
            "droppedCalls": calls.dropped,
            "captureFailures": self.capture_failures.load(Ordering::Relaxed),
        })
    }
}

static HOOKS: Mutex<Vec<Arc<Hook>>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn sites() -> &'static [&'static HookSite] {
    &crate::__private::HOOK_SITES
}

/// Drops hooks nobody has read or waited on for their `ttl`.
fn expire() {
    let expired: Vec<Arc<Hook>> =
        HOOKS.lock().map(|hooks| hooks.iter().filter(|hook| hook.expired()).cloned().collect()).unwrap_or_default();
    for hook in expired {
        hook.remove();
    }
}

fn find(id: u64) -> Result<Arc<Hook>, Error> {
    HOOKS
        .lock()
        .ok()
        .and_then(|hooks| hooks.iter().find(|hook| hook.id == id).cloned())
        .ok_or_else(|| Error::BadCommand(format!("no active hook {id}; see `hooks`")))
}

/// A site by its full path, or a unique `::`-suffix of it.
fn site(path: &str) -> Result<&'static HookSite, Error> {
    if let Some(site) = sites().iter().find(|site| site.path == path) {
        return Ok(site);
    }
    let suffix = format!("::{path}");
    let matches: Vec<&&HookSite> = sites().iter().filter(|site| site.path.ends_with(&suffix)).collect();
    match matches.as_slice() {
        [site] => Ok(site),
        [] => Err(Error::NoSuchFunction(path.to_owned())),
        _ => Err(Error::AmbiguousFunction {
            name: path.to_owned(),
            candidates: matches.iter().map(|site| site.path.to_owned()).collect(),
        }),
    }
}

/// `key=value` options at the start of `text`, and the rest.
fn options(text: &str) -> (Vec<(&str, &str)>, &str) {
    let mut options = Vec::new();
    let mut rest = text.trim_start();
    while let Some((word, after)) = rest.split_once(char::is_whitespace).or(Some((rest, ""))) {
        match word.split_once('=') {
            Some((key, value))
                if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic()) && !value.starts_with('=') =>
            {
                options.push((key, value));
                rest = after.trim_start();
            }
            _ => break,
        }
        if rest.is_empty() {
            break;
        }
    }
    (options, rest)
}

fn number<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, Error> {
    value.parse().map_err(|_| Error::BadCommand(format!("{key}={value} is not a number")))
}

/// `90`, `90s`, `30m`, `2h`.
fn duration(key: &str, value: &str) -> Result<Duration, Error> {
    let (digits, unit) = value.split_at(value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len()));
    let amount: u64 = number(key, digits)?;
    let seconds = match unit {
        "" | "s" => amount,
        "m" => amount * 60,
        "h" => amount * 3600,
        _ => return Err(Error::BadCommand(format!("{key}={value}: use s, m or h"))),
    };
    Ok(Duration::from_secs(seconds))
}

pub(crate) const HELP: &str = "hooks [filter] | hook add <function> [depth=2] [capacity=1000] [self=false] [ttl=30m] \
     | hook read <id> [cursor] | hook wait <id> [timeout=30s] [cursor=n] [<rhai predicate>] | hook remove <id> | hook clear";

/// Runs a `hooks` or `hook ...` command; `None` for any other command.
pub fn command(line: &str) -> Option<Result<Value, Error>> {
    let line = line.trim();
    let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    match verb {
        "hooks" => Some(Ok(list(rest.trim()))),
        "hook" => Some(hook(rest.trim())),
        _ => None,
    }
}

fn list(filter: &str) -> Value {
    expire();
    let filter = filter.to_lowercase();
    let active: Vec<Arc<Hook>> = HOOKS.lock().map(|hooks| hooks.clone()).unwrap_or_default();
    let mut found: Vec<&HookSite> =
        sites().iter().copied().filter(|site| site.path.to_lowercase().contains(&filter)).collect();
    found.sort_by_key(|site| site.path);
    Value::Array(
        found
            .iter()
            .map(|site| {
                let hook = active.iter().find(|hook| std::ptr::eq(hook.site, *site)).map(|hook| hook.id);
                json!({ "path": site.path, "signature": site.signature, "hook": hook })
            })
            .collect(),
    )
}

fn hook(text: &str) -> Result<Value, Error> {
    expire();
    let (action, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let rest = rest.trim();
    let id = || -> Result<(u64, &str), Error> {
        let (id, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        Ok((number("id", id)?, rest.trim()))
    };
    match action {
        "add" => add(rest),
        "read" => {
            let (id, rest) = id()?;
            let hook = find(id)?;
            hook.touch();
            let cursor = if rest.is_empty() { 0 } else { number("cursor", rest)? };
            Ok(hook.read(cursor))
        }
        "wait" => {
            let (id, rest) = id()?;
            wait(&find(id)?, rest)
        }
        "remove" => {
            let (id, _) = id()?;
            let hook = find(id)?;
            let summary = hook.summary();
            hook.remove();
            Ok(summary)
        }
        "clear" => {
            let hooks: Vec<Arc<Hook>> = HOOKS.lock().map(|hooks| hooks.clone()).unwrap_or_default();
            let summaries: Vec<Value> = hooks.iter().map(|hook| hook.summary()).collect();
            for hook in hooks {
                hook.remove();
            }
            Ok(Value::Array(summaries))
        }
        _ => Err(Error::BadCommand(format!("usage: {HELP}"))),
    }
}

fn add(text: &str) -> Result<Value, Error> {
    let (path, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if path.is_empty() {
        return Err(Error::BadCommand("expected a function; see `hooks`".into()));
    }
    let site = site(path)?;
    let (options, extra) = options(rest);
    if !extra.is_empty() {
        return Err(Error::BadCommand(format!("unexpected '{extra}'; options are key=value")));
    }
    let (mut depth, mut capacity, mut capture_self, mut ttl) = (DEFAULT_DEPTH, DEFAULT_CAPACITY, false, DEFAULT_TTL);
    for (key, value) in options {
        match key {
            "depth" => depth = number(key, value)?,
            "capacity" => capacity = number::<usize>(key, value)?.max(1),
            "self" => capture_self = value == "true",
            "ttl" => ttl = duration(key, value)?,
            _ => return Err(Error::BadCommand(format!("unknown option '{key}'; {HELP}"))),
        }
    }
    if let Some(existing) =
        HOOKS.lock().ok().and_then(|hooks| hooks.iter().find(|hook| std::ptr::eq(hook.site, site)).cloned())
    {
        existing.touch();
        return Ok(json!({ "hook": existing.id, "path": site.path, "existing": true }));
    }
    install_panic_hook();
    let hook = Arc::new(Hook {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        site,
        depth,
        capture_self,
        capacity,
        ttl,
        calls: Mutex::new(Calls::default()),
        arrived: Condvar::new(),
        capture_failures: AtomicU64::new(0),
        last_used: Mutex::new(Instant::now()),
    });
    if let Ok(mut slot) = site.hook.lock() {
        *slot = Some(Arc::clone(&hook));
    }
    if let Ok(mut hooks) = HOOKS.lock() {
        hooks.push(Arc::clone(&hook));
    }
    site.active.store(true, Ordering::Relaxed);
    Ok(json!({ "hook": hook.id, "path": site.path, "signature": site.signature }))
}

/// Waits for the first call from `cursor` on (by default, from now) that the predicate accepts.
fn wait(hook: &Arc<Hook>, text: &str) -> Result<Value, Error> {
    let (options, predicate) = options(text);
    let mut timeout = DEFAULT_WAIT;
    let mut cursor = None;
    for (key, value) in options {
        match key {
            "timeout" => timeout = duration(key, value)?.min(MAX_WAIT),
            "cursor" => cursor = Some(number::<u64>(key, value)?),
            _ => return Err(Error::BadCommand(format!("unknown option '{key}'; {HELP}"))),
        }
    }
    let predicate = Predicate::new(predicate)?;
    let deadline = Instant::now() + timeout;
    let mut cursor = cursor.unwrap_or_else(|| hook.calls.lock().map(|calls| calls.next).unwrap_or(0));
    loop {
        hook.touch();
        let page = hook.read(cursor);
        for call in page["calls"].as_array().into_iter().flatten() {
            if predicate.accepts(call)? {
                return Ok(json!({ "satisfied": true, "call": call, "cursor": call["seq"].as_u64().unwrap_or(0) + 1 }));
            }
        }
        cursor = page["cursor"].as_u64().unwrap_or(cursor);
        let now = Instant::now();
        if now >= deadline || !hook.site.active.load(Ordering::Relaxed) {
            return Ok(json!({ "satisfied": false, "cursor": cursor, "calls": hook.summary()["calls"] }));
        }
        let Ok(calls) = hook.calls.lock() else { return Err(Error::Script("the hook's buffer is unusable".into())) };
        if calls.next == cursor {
            let _ = hook.arrived.wait_timeout(calls, (deadline - now).min(Duration::from_millis(500)));
        }
    }
}

/// A `hook wait` condition: a Rhai expression over the call, or none (any call).
struct Predicate {
    #[cfg(feature = "script")]
    compiled: Option<(rhai::Engine, rhai::AST)>,
}

impl Predicate {
    fn new(source: &str) -> Result<Self, Error> {
        if source.trim().is_empty() {
            return Ok(Self {
                #[cfg(feature = "script")]
                compiled: None,
            });
        }
        #[cfg(feature = "script")]
        {
            let engine = crate::script::predicate_engine();
            let ast = engine.compile_expression(source).map_err(|error| Error::Script(error.to_string()))?;
            Ok(Self { compiled: Some((engine, ast)) })
        }
        #[cfg(not(feature = "script"))]
        Err(Error::BadCommand("predicates need scry-rs's `script` feature, which this build turned off".into()))
    }

    fn accepts(&self, call: &Value) -> Result<bool, Error> {
        #[cfg(feature = "script")]
        if let Some((engine, ast)) = &self.compiled {
            return crate::script::predicate(engine, ast, call);
        }
        let _ = call;
        Ok(true)
    }
}
