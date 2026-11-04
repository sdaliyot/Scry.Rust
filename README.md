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

## What gets generated

`cargo scry` compiles a rewritten copy of each opted-in crate. The copy and a `scry-report.txt` listing
what was generated and skipped are kept under `target/scry/<profile>/scry-src/<crate>/`. The copy keeps
every line where it was, so errors, panics and the debugger still point at the original files. It adds:

- `#[derive(Reflect)]` on every struct and enum, including types defined by the crate's own `macro_rules!`
- a registration for every non-`mut` static, by its full path (`console_demo::APP`, also reachable as `APP`)
- reflection through `Box<dyn Trait>` for every trait that can be used as `dyn Trait`
- for Dioxus crates, tracking of `use_signal`/`use_signal_sync`/`use_context_provider`/`use_memo` results under `ui.<Component>.<binding>`
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
- Function and method invocation isn't implemented yet.
- `cargo scry clippy` doesn't work, because clippy uses the same wrapper slot.
