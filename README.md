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
- not generic
- not `async` or `unsafe`
- no `&mut` or `impl Trait` parameters
- not `main`, a test or a UI component

Functions behind unknown attribute macros are skipped, because the macro may rewrite them.
`scry-report.txt` lists every skipped function and why.

## What gets generated

`cargo scry` compiles a rewritten copy of each opted-in crate. The copy and a `scry-report.txt` listing
what was generated and skipped are kept under `target/scry/<profile>/scry-src/<crate>/`. The copy keeps
every line where it was, so errors, panics and the debugger still point at the original files. It adds:

- `#[derive(Reflect)]` on every struct and enum, including types defined by the crate's own `macro_rules!`
- a registration for every non-`mut` static, by its full path (`console_demo::APP`, also reachable as `APP`)
- reflection through `Box<dyn Trait>` for every trait that can be used as `dyn Trait`
- for Dioxus crates, tracking of `use_signal`/`use_signal_sync`/`use_context_provider`/`use_memo` results under `ui.<Component>.<binding>`
- a call shim for every callable function and method (see above)
- the endpoint, started at the top of `main` (and, for Dioxus, pumped from the launched root component)

The endpoint listens on `127.0.0.1` on a port the OS picks. It writes a descriptor holding the pid, the
port and a random token to `%USERPROFILE%\.scry-rs\targets` (`~/.scry-rs/targets` elsewhere;
`SCRY_RS_TARGETS` overrides it). Every request must carry that token. It is not under `%LOCALAPPDATA%`
because Windows redirects `AppData` for processes started from packaged apps such as Claude Desktop.

Writes need a `Mutex`, `RwLock`, atomic or signal somewhere on the path. Anything else is refused
with `not_writable`, because writing through a shared reference is undefined behaviour in Rust.

## Known limits

- Plain stack locals can't be reached. Only statics, and state reachable from them or from tracked hooks.
- Types from other crates show as `Debug` text or a type name, except the std types and Dioxus signals `scry-rs` supports.
- Types created by third-party macros (Dioxus props, `bitflags!`) aren't rewritten.
- Generic functions and methods of generic types can't be called yet: choosing the type arguments
  needs type information the generator doesn't have.
- Called code runs on the registry's thread (the UI thread in a Dioxus app), and the script time limit
  doesn't stop native code. A function that blocks or loops blocks that thread. So does a method
  that locks a `Mutex` its own path already holds.
- Only types defined without generic parameters in opted-in crates have callable methods.
- Script variables don't survive between `eval` calls.
- `cargo scry clippy` doesn't work, because clippy uses the same wrapper slot.
