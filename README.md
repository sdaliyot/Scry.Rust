# Scry.Rust (POC)

Inspect and change the state of a running Rust process, with no Scry code in the app's sources.

## Layout

| Folder | What |
|---|---|
| `scry/crates/scry-rs` | The runtime library (imported as `scry`): `Reflect`, `Registry`, the endpoint |
| `scry/crates/scry-rs-derive` | `#[derive(Reflect)]` |
| `scry/crates/scry-rs-cargo` | `cargo scry`: builds an app with a generated registry |
| `scry/crates/scry-rs-cli` | `scry`: the command line that talks to running apps |
| `demos/` | A console app and a Dioxus desktop app to try it on |

None of the crates is published yet. `scry` is taken on crates.io; `scry-rs` and `scry-rs-derive` are free.

## Opting an app in

Only the manifest changes:

```toml
[features]
scry = ["dep:scry"]

[dependencies]
scry = { package = "scry-rs", path = "…/scry/crates/scry-rs", optional = true }
# A Dioxus app adds: features = ["dioxus"]
```

A normal `cargo build` is unchanged. `cargo scry build` (or `run`, `test`, …) builds the same code with
the registry generated in. It writes to `target/scry`, so the normal and Scry builds don't rebuild each other's dependencies.
`cargo scry clippy [args] [-- lint args]` lints the rewritten code. The generated parts allow every
lint group, so only the app's own code is reported.

## Trying it

```powershell
cd scry; cargo build -p scry-rs-cargo -p scry-rs-cli; cd ..\demos
..\scry\target\debug\cargo-scry.exe scry run -p console-demo
```

In another terminal (`scry` is `scry\target\debug\scry.exe`):

```powershell
scry list
scry roots
scry get APP.stats
scry set APP.rules[0].percent 50
scry set DISCOUNTS_ENABLED false
```

With `cargo-scry.exe` and `scry.exe` on `PATH` (for example via `cargo install --path`), the first command
becomes `cargo scry run -p console-demo`.

For `dx-demo`, the component signals are under `ui`: `scry get ui`, `scry set ui.App.uptime 1000`,
`scry set FILTER Done`.

## Scripts

`scry eval <script>` runs a [Rhai](https://rhai.rs) script in the app (`scry eval --file x.rhai`, or
`scry eval -` for stdin). Roots are variables, members are reached as in Rust, and assignments write
through the same rules as `set`:

```rhai
for rule in APP.rules { print(rule.type_name()); }
APP.rules[0].percent = 25;
APP.stats.ticks += 1000;
if FILTER == "All" { FILTER = "Done"; }
#{ items: APP.inventory.items.len(), first: APP.inventory.items[0].name }
```

The reply's `result` is the script's last value as JSON, and `output` holds what it printed.

- A value with members (a struct, a list, a map) reads as a live handle that is re-read on every
  access.
- Everything else reads as a plain copy.
- Handles also have methods: `get()` / `get(depth)` for a snapshot, plus `set(v)`, `len()`,
  `members()`, `children()`, `type_name()`, `kind()` and `path()`.
- A root without members, such as `FILTER`, is a plain variable. It is written back when the script
  ends without an error.

A script runs on the registry's thread, which is the UI thread in a Dioxus app. It is stopped after
5 seconds, can't load modules, and has bounded string, array and map sizes. Scripting comes from
`scry-rs`'s default `script` feature.

## Waiting and asserting

`wait` and `assert` check a Rhai condition against the app's state. A test or an agent can gate on
the app reaching a state, or fail when it hasn't:

```powershell
scry wait timeout=15 'APP.stats.sold >= 10'        # {"satisfied": true, "attempts": 26, "elapsedMs": 2750}
scry wait timeout=1 'APP.inventory.len() == 99'    # {"satisfied": false, "lastValue": false, ...}
scry assert 'FILTER == "All"'                      # fails with assertion_failed when it doesn't hold
```

- **`wait`** evaluates the condition every `interval` (100 ms by default) until it gives `true`, or
  `timeout` passes (10 s by default, at most 5 min).
- **A timeout is not an error.** The reply says `satisfied: false` with the last value or error, so
  read the flag rather than the exit code.
- **Errors while waiting count as "not yet".** That covers a path that doesn't exist yet or a busy
  lock.
- **`assert`** evaluates once.
- **A condition that doesn't compile** fails at once, for both.
- **The UI isn't blocked.** Through the endpoint, each evaluation is one short trip to the registry
  thread, and the waiting happens elsewhere, so a long wait never holds a Dioxus app's UI thread.

## Calling app code

`cargo scry` also makes the app's functions and methods callable, with no attributes needed:

```powershell
scry fns inventory                          # callable functions, filtered by path
scry ls APP.inventory                       # members, and the methods of the value there
scry call APP.inventory.total_value_cents   # a method on the live value
scry call util::format_cents 12345          # a free function; arguments are JSON values
scry call Discount::new '"promo"' 15        # an associated function (a constructor)
```

In scripts, methods use method syntax and free functions are called by name when the name is
unique. `invoke("path", [args])` reaches any of them:

```rhai
let inv = APP.inventory;
inv.add("Gizmo", 1999, 3, "Misc");          // &mut self, behind the app's RwLock
APP.rules[0].apply(1000, inv.items[0])      // a trait method through Box<dyn PriceRule>
format_cents(inv.total_value_cents())
```

- A method runs on the live value at that path while the app's locks on the way are held. A
  `&mut self` method needs a lock, atomic or signal on its path, like a write.
- A `self` method runs on a clone, so it needs `Clone`.
- Arguments are built from JSON in the shapes `get` returns. Passing a node passes a copy of its
  value.
- Results come back as snapshots. A panic is caught and reported as `panicked`.

Callable means:
- module-level functions, and methods and associated functions of module-level `impl` blocks
  (inherent or trait)
- not generic, or generic and called by the crate with concrete types (see below)
- not `async` or `unsafe`
- no `&mut` or `impl Trait` parameters
- not `main`, a test or a UI component

Functions behind unknown attribute macros are skipped, because the macro may rewrite them.
`scry-report.txt` lists every skipped function and why.

Scripts call a value's methods with method syntax. When the type has its own method with the same
name and argument count as one of a node's methods (`Cache::get(key)`, `Inventory::len()`), the
app's method wins. `get(node.path())` still takes a snapshot.

### Generic functions and types

A generic function or impl gets one shim for each set of concrete types the crate itself calls it
with. To find those, `cargo scry` has rustc compile the crate once more to MIR only (the stable
`--emit=mir`) and reads the calls from it:

```powershell
scry fns describe                           # util::describe::<Item>, util::describe::<Stats>
scry call ratio 1 4                         # the only instantiation answers to the plain name
scry eval 'APP.labels.get(999)'             # Cache<u32, String>::get on the live cache
```

- An instantiation is listed with its type arguments, such as `describe::<Item>`. When there are
  several, call it by that path.
- Only calls count, because a call proves the types meet the function's bounds. For a generic
  `impl`, a call to any of its methods covers the whole block, except methods with `where` bounds
  of their own.
- Each instantiation's type arguments must be nameable from the function's module: a crate type
  visible there, a std type, or a primitive. Closures, and types private to another module, drop
  that instantiation.
- The extra MIR pass runs only for crates that have generic items. On the demo it adds about
  2 seconds to a rebuild.

## Hooking calls

Every hookable function in a `cargo scry` build has a hook point compiled in. `scry hook add` turns
on recording for one function: each call's arguments, result or panic, duration and thread.

```powershell
scry hooks inventory                                   # hookable functions, and which are hooked
scry hook add Inventory::sell_one self=true            # returns {"hook": 1, ...}
scry hook wait 1 timeout=60 'result != () && args.index == 2'
scry hook read 1                                       # every recorded call, with a cursor for the next read
scry hook remove 1
```

- **Snapshots:** arguments and results are snapshots taken during the call, `depth=2` levels deep by
  default, with `self` when `self=true`. A buffer keeps the last `capacity=1000` calls.
- **Panics and async:** a panic is recorded with its message and then continues as before. An
  `async fn` is recorded when its work completes.
- **Waiting:** `hook wait` takes `key=value` options, then an optional Rhai predicate over `args`,
  `result`, `panic`, `instance` (the captured `self`), `seq`, `duration_micros` and `thread`. It
  waits on the endpoint's connection thread, so a long wait never holds a Dioxus app's UI thread.
- **Scripts** can use `hook_add`, `hook_read` and `hook_remove`.
- **Expiry:** a hook expires after `ttl=30m` without a `read` or `wait`.
- **Cost:** while no hook is on a function, its hook point costs one atomic load per call. That was
  below what a release-build benchmark could measure.
- **Capturing never disturbs the function.** A busy lock shows as `<locked: ...>` rather than being
  waited for, a failed capture is only counted, and calls made while capturing aren't recorded.

### Changing calls

A hook can also change the calls it sees. Use this for fault injection ("make the next sale fail"),
for stubbing a slow dependency, or for steering the app into a state that's hard to reach:

```powershell
scry hook before 1 'if args.index == 0 { #{ args: #{ index: 4 } } }'   # change an argument
scry hook before 2 '#{ result: () }'                                    # skip the body, return ()
scry hook after 3 'if args.id == 7 { #{ result: #{ Err: "injected" } } }'   # replace the result
scry hook before 1                                                      # an empty script removes it
```

- **When scripts run:** a `before` script runs on the calling thread before the body, with `args`
  and `instance`. It returns `()` for no change, or a map with `args`, `instance` (the `&mut self`
  receiver) or `result`; a `result` skips the body and is returned instead. An `after` script also
  sees `result`, and can return `#{ result: v }` to replace it.
- **What can change:**
  - By-value and `&mut` arguments, and a `&mut self` receiver, are written the way `set` writes:
    a struct takes the fields to change.
  - `&T` arguments can't be changed. Writing through a shared reference would be undefined
    behaviour.
  - A result is built from JSON when its type allows (`{"Ok": ..}`/`{"Err": ..}` for a `Result`).
    Otherwise it's written into the real result.
- **Records:** each changed call is recorded with `changes`, and `skipped: true` when the body was
  skipped.
- **Failures:** a change that fails (a type mismatch, or a script error or panic) leaves the call as
  it was. It is recorded in `changeErrors` and counted in `changeFailures`.
- **Limits:** scripts stop after 100,000 operations, because the app's own thread waits for them.
  They're compiled once per thread, so the first changed call on a thread takes a few
  milliseconds.
- **Marking:** a hook with change scripts is marked `intervenes` in `hooks`, and its summary shows
  the scripts. Treat it like `set`: it changes the app.

Hooks are on by default. A crate can turn them off, or exclude hot or sensitive functions, in its
`Cargo.toml` (`[workspace.metadata.scry.hooks]` sets the same keys for a whole workspace):

```toml
[package.metadata.scry.hooks]
enabled = true                                  # false builds the crate without hook points
exclude = ["crate::util::next_random", "crate::auth::*"]
```

**No hook point**, each listed in `scry-report.txt` with the reason:
- `const` and `unsafe` functions, `#[track_caller]`
- functions returning `!` or `impl Trait`
- a body starting with an inner attribute
- `main`, tests, UI components, and functions under unknown attribute macros

If a crate doesn't compile with its hook points, `cargo scry` builds it again without them and warns.
`SCRY_VERBOSE=1` shows why. See [docs/design/hooks.md](docs/design/hooks.md) for the design.

## What gets generated

`cargo scry` compiles a rewritten copy of each opted-in crate. The copy and a `scry-report.txt` listing
what was generated and skipped are kept under `target/scry/<profile>/scry-src/<crate>/`. The copy keeps
every line where it was, so errors, panics and the debugger still point at the original files. It adds:

- `#[derive(Reflect)]` on every struct and enum, including types defined by the crate's own `macro_rules!`
- a registration for every non-`mut` static, by its full path (`console_demo::APP`, also reachable as `APP`)
- reflection through `Box<dyn Trait>` for every trait that can be used as `dyn Trait`
- for Dioxus crates, tracking of `use_signal`/`use_signal_sync`/`use_context_provider`/`use_memo` results under `ui.<Component>.<binding>`
- a call shim for every callable function and method (see above)
- a hook point in every hookable function (see above)
- the endpoint, started at the top of `main` (and, for Dioxus, pumped from the launched root component)

The endpoint listens on `127.0.0.1` on a port the OS picks. It writes a descriptor holding the pid, the
port and a random token to `%USERPROFILE%\.scry-rs\targets` (`~/.scry-rs/targets` elsewhere;
`SCRY_RS_TARGETS` overrides it). Every request must carry that token. It is not under `%LOCALAPPDATA%`
because Windows redirects `AppData` for processes started from packaged apps such as Claude Desktop.

Writes need a `Mutex`, `RwLock`, atomic or signal somewhere on the path. Anything else is refused
with `not_writable`, because writing through a shared reference is undefined behaviour in Rust.

A panic during a command doesn't take the app down. This covers app code it calls, a `Debug` impl
it reads through, or a Dioxus global signal read without the runtime (that one is `no_runtime`).
- The command fails with `panicked`, and the app and the endpoint carry on.
- A lock Scry held at the time is not left poisoned, so the app's own `.lock().unwrap()` keeps
  working.
- Two cases still end the process: a build with `panic = "abort"` (the endpoint warns about this
  when it starts), and an app panic hook that exits.

Type names are shown short (`Vec<Item>`); `ls` also gives the full one as `fullType`.

## Known limits

- Plain stack locals can't be reached. Only statics, and state reachable from them or from tracked hooks.
- Types from other crates show as `Debug` text or a type name, except the std types and Dioxus signals `scry-rs` supports.
- Types created by third-party macros (Dioxus props, `bitflags!`) aren't rewritten.
- A generic function or type can only be called with type arguments the crate itself uses with
  it. Instantiations that only appear inside other generic code, or that need a closure type, are
  not found.
- Called code runs on the registry's thread (the UI thread in a Dioxus app), and the script time limit
  doesn't stop native code. A function that blocks or loops blocks that thread. So does a method
  that locks a `Mutex` its own path already holds.
- Only types defined without generic parameters in opted-in crates have callable methods.
- Script variables don't survive between `eval` calls.
