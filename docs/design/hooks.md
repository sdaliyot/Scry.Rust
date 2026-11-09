# Design: method hooks for Scry.Rust

Status: implemented. That covers phase 1 (observing calls, and the `Cargo.toml` configuration) and
phase 2 (changing calls). Where the implementation differs from the plan below, the notes in "As
built" say how.

## As built

- **`hook wait` options** are `key=value` words before the predicate:
  `hook wait 1 timeout=60 cursor=0 args.index == 2`.
- **Predicate variables:**
  - `self` is a Rust keyword and `this` is Rhai's, so the captured `self` is `instance`.
  - The others are `args`, `result`, `panic`, `cancelled`, `seq`, `duration_micros`, `thread`,
    and `call` for the whole record.
- **Scripts** get `hook_add`, `hook_read` and `hook_remove`, but not `hook_wait`. A script runs on
  the registry's thread, the UI thread in a Dioxus app, so waiting there would stop the very calls
  it waits for.
- **The body runs through `run_once(move || ...)`** rather than calling the closure directly. A
  closure called directly is inferred `FnMut`, which cannot return what it reborrows from `&mut`
  parameters (`fn first_mut(&mut self) -> Option<&mut T>`). The result is declared with the
  function's type, which an `async fn`'s `?` and the result probe both need.
- **Build safety net:**
  - It works as planned.
  - `SCRY_VERBOSE=1` shows the output of attempts that were thrown away.
  - It already caught a lint that `cargo scry clippy -- -D warnings` turned into an error inside a
    hook point, and that lint is now allowed there.
- **Cost of an inactive hook point:** within the noise of a 4 ns/call benchmark loop in a release
  build, so a fraction of a nanosecond.
- **Changing calls (phase 2):**
  - **Commands:** `hook before <id> <rhai>` and `hook after <id> <rhai>` attach scripts to an
    existing hook, and an empty script removes one. Scripts are checked when attached, so a syntax
    error is refused up front.
  - **Return values:** a before script returns `#{ args, instance, result }`; an after script
    returns `#{ result }`.
  - **Where they run:** on the calling thread, compiled once per thread, and stopped after 100,000
    operations.
  - **Records** carry `changes`, `skipped` and `changeErrors`; hooks count `changeFailures`, and
    those with scripts are marked `intervenes`.
  - **Writing arguments:** by-value parameters are rebound as `mut` so they can be written. `&T`
    parameters and by-value `self` are never offered for writing.
  - **Building a replacement:** `FromValue` is tried first; if that fails, the value is written into
    the existing result. `FromValue` for `Result` was added for `#{ Err: .. }`.
- **Config changes rebuild:** the manifests read for `[package.metadata.scry]` are added to the
  build's dependency file, so editing `exclude` rebuilds the crate. Cargo ignores metadata changes
  otherwise.

## Goal

Scry.NET's hooks, in Rust: record every call to a chosen function or method in a running app,
including what it was given, what it returned, whether it panicked, how long it took and on which
thread, without changing what the function does. Then read those calls, or wait for one that
matches a condition.

```text
scry-rs hook add console_demo::inventory::Inventory::sell_one
scry-rs hook wait 1 "result != () && args.index == 2" 60
scry-rs hook read 1
scry-rs hook remove 1
```

Like everything else in `cargo scry`, it needs no change to the app's sources.

## The core difference from Scry.NET

Scry.NET patches a method's machine code at run time, using Harmony. A Rust binary can't be patched
safely that way: there is no runtime metadata, no JIT, and calls are often inlined. So the hook
point is **compiled in**: `cargo scry` instruments every hookable function, and `hook add` only
switches recording on for one of them.

That brings advantages Harmony can't offer:

- **Inlining doesn't matter.** The recording code is part of the function's own body, so it runs
  wherever the function is inlined. Scry.NET's "inlining risk" warnings have no equivalent.
- **No executable-memory patching**, so there's nothing for EDR or antivirus to flag.
- **`async fn` is recorded when its work completes,** with the real output, not when it returns
  its future.
- **Generic functions are covered at every instantiation,** not only the ones the app calls.

And two costs:

- **A function can only be hooked in a `cargo scry` build.** That is already true of everything else
  in Scry.Rust.
- **Every hookable function pays a check per call while it isn't hooked:** one relaxed atomic load
  and a branch. That is on the order of a nanosecond. I'll measure it on the demos and report it.

## What the generator emits

For a function such as

```rust
pub fn sell_one(&mut self, index: usize) -> Option<Item> {
    BODY
}
```

the body is wrapped in place. The prefix goes on the line of the opening `{` and the suffix on the
line of the closing `}`, so every line of `BODY` stays where it was:

```rust
pub fn sell_one(&mut self, index: usize) -> Option<Item> { static SITE: HookSite = ...; /* registered with linkme */
    let __scry_call = SITE.enter(|capture| { capture.arg("index", &index); capture.this(&self); });
    let __scry_result = (move || -> Option<Item> {
    BODY
    })(); __scry_call.exit(&__scry_result); __scry_result }
```

- **`enter`** loads the site's `active` flag. While it's off, nothing else happens: the capture
  closure isn't even called. While it's on, it snapshots the arguments (and `self`, if the hook asked
  for it) to JSON, records the start time and thread, and returns a guard.
- **The body runs in a `move` closure** declared with the function's own return type. So `return`
  and `?` mean exactly what they meant before, and parameters move in as they would have.
- **`exit`** snapshots the return value. If a panic unwinds instead, the guard's `Drop` records the
  call as panicked, with the message, taken from a chained panic hook installed only while some hook
  is active.
- **`async fn`** uses `async move { BODY }.await` instead of a closure, so the call is recorded when
  the future completes. The guard is `Send`, so a `Send` future stays `Send`.
- **The site** is a `static` inside the function, so a generic function has one site covering every
  instantiation. Each site carries the function's path and signature, and is registered in a linkme
  list like the call shims, so `scry-rs hooks` can list it.

### Which functions are hookable

The set is wider than for calls, because nothing has to be built from JSON:

- module-level functions, and the methods of module-level `impl` blocks, inherent or trait
- including generic functions, `async` functions, `&mut` and `impl Trait` parameters, and an
  `impl Trait` return type (left to inference; a script can't replace such a result whole)

**Not hookable in this version,** each listed in `scry-report.txt` with the reason:

| Kind | Why |
|---|---|
| `const fn` | Closures aren't allowed in `const fn` |
| `unsafe fn` | Unsafe operations inside a closure need their own `unsafe` block |
| `#[track_caller]` | The closure would change what `Location::caller()` reports |
| `-> !` functions | A closure can't be declared `-> !` on stable Rust |
| Trait default method bodies; functions nested in other functions | Out of scope for now |
| Dioxus components, `main`, tests | The same rules as for calls |
| Under an attribute macro that breaks it | Tried, and left out if it doesn't compile (see below) |

### Safety net for the build

A body wrapped in a closure compiles in the overwhelming majority of cases, but the borrow checker
has corners where it doesn't. To make sure that can never break someone's build:

1. If the rewritten crate fails to compile, the wrapper traces each error to the call shims and
   hook points it points at - through the macro expansions it came from, so an attribute macro
   that rewrites a function is caught too - and compiles the crate again without just those. What
   was left out is remembered for the next build. This is also what lets every function under an
   attribute macro be tried, rather than skipped: only what doesn't compile under it goes.
2. If errors point at no shim or hook point, the wrapper compiles the crate again with hooks off,
   keeping everything else.
3. If that fails too, it compiles the original sources, as it already does when the rewrite itself
   fails.
4. It warns which step it fell back to, and shows only the errors of the last attempt. So a real
   error in the app's own code is reported once, against the original files.

The retries happen only on failure, so a build that compiles costs nothing extra.

## Capturing values

- **Snapshots, not live references.** Arguments, `self` and results become JSON snapshots at call
  time, `depth` levels deep (default 2), through the same probes as `get`. A value without `Reflect`
  shows as `Debug` text or its type name.
  - Rust gives no safe way to keep a borrowed argument alive after the call. Scry.NET's equivalent
    is a type-only preview unless `includeReferences` is set.
  - Snapshots record what the value was at that moment, which is usually what you want.
- **Snapshots never wait for locks.** While capturing, a `Mutex`/`RwLock`/signal is tried once and
  shows as `"<locked>"` if it's busy. A capture never stalls the app's thread, and can't deadlock
  on a lock the calling thread already holds.
- **Never breaks the hooked function.**
  - Capture runs under `catch_unwind`; a failure is counted in `captureFailures`.
  - A thread-local flag stops calls made *during* a capture, such as a hooked function used by a
    `Debug` impl, from being recorded or recursing.
  - The hooked function itself always runs exactly as before.
- **Costs only while on.** Snapshot costs are paid only by hooked functions, and only while their
  hook is active.

## Records and buffer

Each active hook keeps a ring buffer of its last `capacity` calls (default 1000). Each call record:

```json
{ "seq": 41, "start": "2026-11-05T10:12:03.120Z", "durationMicros": 85,
  "thread": "scry-worker (1234)", "args": { "index": 2 }, "self": { ... },
  "result": { "name": "Hammer", "qty": 3 } }
```

A panicked call has `"panic": "message"` instead of `result`. A call still running shows in neither
`read` nor `wait` until it ends.

Calls dropped from a full buffer are counted (`droppedCalls`). A `read` whose cursor is behind the
buffer says `truncated: true`. Buffer and counters are thread-safe, because hooked functions run on
any thread.

## Commands

| Command | What it does |
|---|---|
| `hooks [filter]` | Hookable functions, with path and signature; active hooks are marked |
| `hook add <path> [depth=2] [capacity=1000] [self=false] [ttl=30m]` | Turns recording on for one function and returns a hook id. Adding a function that is already hooked returns the existing id. `path` takes a unique `::`-suffix, as `call` does. |
| `hook read <id> [cursor=0]` | Calls from `cursor` on, with the next cursor, `droppedCalls`, `captureFailures` and `truncated` |
| `hook wait <id> [timeout=30s] [cursor=n] [predicate]` | Waits for the first call from `cursor` on (default: from now) for which the Rhai `predicate` holds; see "As built" for its variables. Returns the call, or `satisfied: false` when it times out |
| `hook remove <id>` / `hook clear` | Turns recording off and drops the buffer |

Three points worth calling out:

- **`hook wait` doesn't run on the registry thread.** Records are plain JSON, so the endpoint's
  connection thread polls the buffer and evaluates the predicate itself. A long wait never holds a
  Dioxus app's UI thread. Scry.NET makes the same guarantee for its predicates.
- **A hook expires** after `ttl` without a `read` or `wait`. Scry.Rust has no sessions, so a hook
  that someone forgot doesn't go on costing capture time forever. Scry.NET ties a hook's life to
  its session instead.
- **Scripts** get `hook_add`, `hook_read` and `hook_wait` too. A script can then set up a hook,
  call app code and inspect the recorded calls in one round trip.

## Plan

1. **Runtime** (`scry-rs`):
   - hook sites, the linkme list, `enter`/`exit`/guard, ring buffers
   - the panic-message capture
   - the no-wait capture mode, the recursion guard and `catch_unwind`
2. **Generator:** instrument the hookable functions (closure or `async` form), report the
   exclusions, and add the build safety net.
3. **Commands and CLI:** `hooks` and `hook add/read/wait/remove/clear`, with `wait` served off the
   registry thread, plus the script functions.
4. **Verification:**
   - generator unit tests for every shape in the tables above
   - runtime tests: panics, recursion, busy locks, the buffer
   - end to end on both demos: hook `Inventory::sell_one`, which the console demo's worker calls on
     its own thread, and `Board::toggle` in the Dioxus demo
   - the cost per call of an inactive hook, measured
5. **Docs:** README section, limits.

## Configuration

Hooks are on by default. A crate turns them off, or excludes hot or sensitive functions, in its own
`Cargo.toml`, so the choice is versioned with the code rather than living in someone's environment:

```toml
[package.metadata.scry.hooks]
enabled = true                       # the default; false builds this crate without hook points
exclude = [
    "crate::util::*",                # every function in a module, and its submodules
    "crate::inventory::Inventory::len",
]
```

`[workspace.metadata.scry.hooks]` sets the same keys for every crate in the workspace, and a crate's
own table overrides it. Patterns are crate-relative paths, as the report prints them; `*` matches one
or more trailing segments. An excluded function has no hook point at all, so it costs nothing. The
report lists it as excluded by configuration.

The `[package.metadata.scry]` table is meant to grow. Opt-in/opt-out for types (`mode`, `exclude`),
discussed earlier and deferred, would go next to `hooks` without changing it.

## Later: changing calls, not only observing them

Observing comes first. A second phase can let a hook change a call, which also makes hooks a tool
for fault injection ("make `charge_card` fail") and for stubbing. Most of it fits the same hook
point:

- **Results:** after the body runs, the result is the hook's to change before it's returned. It can
  be written through `Reflect`, the same path as `set` (`{"Ok": ...}` to `{"Err": ...}` needs the
  enum's `FromValue`; field changes don't).
- **Arguments:**
  - `&mut` and by-value arguments can be changed the same way before the body runs.
  - `&T` arguments can't be changed: they're borrowed, and writing through them would be undefined
    behaviour.
- **Skipping the body:** returning a value without running the body needs `FromValue` for the return
  type. A type without it is refused when the hook is added, not at call time.
- **How changes are expressed:** a Rhai script given to `hook add`, run on the calling thread at
  call time, that sees `args` (and `result`) and returns the changes. It must be short, because the
  app's thread waits for it. It is time-limited like `eval`, and a failure leaves the call
  unchanged.
- **Policy:** a changing hook is a write to the app, so `hook add` marks it as such
  (`"intervenes": true`), and the CLI and the agent skill treat it like `set`.

Estimate: moderate. The hook point, value capture and Rhai are already there; the new parts are
writing back into arguments and results, and running Rhai on the calling thread.

## Sessions: not now

Scry.NET's sessions give three things:
- **leased handles** to objects that have no path
- **cleanup** of hooks and jobs when a client disconnects
- **a scope** that groups one client's work

For Scry.Rust today:

- **Handles** can't safely outlive a request for borrowed values. That is the same reason captures
  are snapshots. Only `Arc`-shared values could be kept, by cloning the `Arc`.
- **Cleanup:** the idle expiry covers forgotten hooks.
- **Persistent script variables** are the one user-visible gain: an `eval` REPL keeping its
  variables across calls. That can be offered on its own, as named script scopes, without sessions.

So no sessions for now. They become worth it with jobs, multi-target scenarios, or a client library
that holds a long-lived connection, which is where Scry.NET uses them most.

## Decisions taken

1. Instrument every hookable function by default. Turning hooks off, or excluding functions, is set
   in `Cargo.toml` (above), not an environment variable.
2. Capture snapshots at call time. Changing arguments and results was built as a second phase.
3. Hooks expire after 30 minutes without a `read` or `wait`. No sessions for now.
