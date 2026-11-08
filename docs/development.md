# Development

How the repository is laid out, how to build and test it, and how a release is made. The
[README](../README.md) covers using Scry.Rust; this document covers working on it.

## Layout

| Folder | What |
|---|---|
| `scry/` | The Scry.Rust workspace |
| `scry/crates/scry-rs` | The runtime library, imported as `scry`: `Reflect`, `Registry`, the endpoint, scripts, hooks, the Dioxus support |
| `scry/crates/scry-rs-derive` | `#[derive(Reflect)]` |
| `scry/crates/scry-rs-cargo` | `cargo scry`: builds an app with a generated registry |
| `scry/crates/scry-rs-cli` | `scry-rs`: the command line that talks to running apps |
| `demos/` | A console app and a Dioxus desktop app, in a workspace of their own |
| `skills/scry-rs/` | The agent Skill |
| `docs/` | This document, the [threat model](threat-model.md), and [design notes](design/) |

The demos are a separate workspace on purpose. They depend on `scry-rs` the way an outside app does,
through a dependency, rather than as a sibling crate of the same workspace.

## Building and testing

You need Rust 1.91 or later. On Linux, the Dioxus desktop parts (the `dioxus-desktop` feature and
the `dx-demo`) also need the WebKitGTK development packages, for example on Ubuntu:

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev libayatana-appindicator3-dev librsvg2-dev
```

From `scry/`:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

To try a change end to end, build `cargo scry` and run a demo with it, from `demos/`:

```sh
cargo build --manifest-path ../scry/Cargo.toml -p scry-rs-cargo -p scry-rs-cli
../scry/target/debug/cargo-scry scry run -p console-demo
../scry/target/debug/scry-rs list
```

[`scripts/e2e.sh`](../scripts/e2e.sh) does the same as a test: it builds both demos with
`cargo scry`, runs them, and drives them with `scry-rs`, checking state, calls, hooks, `wait`, the
page and screenshots. Run it from the repository root (on Windows, from Git Bash). On Linux without
a desktop, run it under Xvfb: `xvfb-run -a scripts/e2e.sh`.

CI ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) runs the three commands above and
builds both demos with `cargo scry` on every platform a release supports: Windows, Linux and
macOS, each on x64 and Arm64. It runs `scripts/e2e.sh` on each of them too (on Linux under Xvfb),
and checks the workspace with Rust 1.91.

## How `cargo scry` works

`cargo scry <command> [args]` runs `cargo <command> --features scry [args]` with the `cargo-scry`
binary itself as the rustc wrapper (`RUSTC_WORKSPACE_WRAPPER`), and with the target directory set to
`target/scry`.

For each workspace crate that has the `scry` feature on and an `--extern scry`, the wrapper
compiles a rewritten copy of the crate's sources instead of the originals. Every other crate, and
every build without `cargo scry`, compiles untouched. The copy:

- keeps every line where it was, and rustc maps its paths back to the originals
  (`--remap-path-prefix`), so errors, panics and debug info point at the real files
- is kept under `target/scry/<profile>/scry-src/<crate>-<hash>/`, with a `scry-report.txt` listing
  what was generated and skipped

The generator's modules, in `scry-rs-cargo/src`:

| Module | What it does |
|---|---|
| `wrapper` | Decides which crates to rewrite, and runs rustc on the copy |
| `rewrite` | Writes the copy of a crate: one file at a time, with the edits below |
| `generate` | Derives, static and const registrations, trait object reflection, Dioxus hook tracking, the endpoint |
| `functions` | Call shims, so `call` and scripts can run the app's functions |
| `hookpoints` | Hook points around function bodies |
| `generics` | Finds the concrete types a crate calls its generic items with, from a MIR-only compilation |
| `features` | Turns on `scry-rs` features and dependencies' serde features to match the app |
| `config` | Reads `[package.metadata.scry]` and `[workspace.metadata.scry]` |
| `edits`, `tokens` | Places text edits by the positions of parsed tokens |

`cargo scry dx ...` runs the Dioxus CLI instead of cargo. `dx` takes the rustc wrapper slot for
itself, so `cargo scry` puts a copy of itself named `rustc` first on the `PATH` it passes to `dx`.
The README's "Apps built with `dx`" section explains the details.

If a crate doesn't compile with its hook points, `cargo scry` builds it again without them and
warns. `SCRY_VERBOSE=1` shows why, along with the features it turns on.

[`design/hooks.md`](design/hooks.md) explains the design of hooks.

## The runtime

`scry-rs` has two rules that shape its API, both forced by Rust:

- **Access is callback-based.** A value behind a lock or a signal can't be returned past its guard,
  so each read and write runs in a callback while the guard is held.
- **Writes go through the app's own synchronization.** The registry has only shared access to its
  roots, so a write needs a lock, an atomic or a signal on its path. A path without one is refused,
  because writing through a shared reference would be undefined behavior.

Commands run on the registry's thread, which is the UI thread in a Dioxus app. The endpoint's
connection threads hand each command to that thread and wait for the reply. `wait` and the hook
commands do their waiting on the connection thread, so they never hold the UI thread.

The endpoint's protocol is one JSON object per line each way:

```text
-> {"token": "...", "command": "get APP.stats 2"}
<- {"ok": true, "result": {...}}
```

The `command` string is exactly what follows `scry-rs` on the command line. The descriptor in
`~/.scry-rs/targets` gives the address and the token. A test can speak this protocol directly, or
run the `scry-rs` command line.

## The icon

`scry-rs.exe` gets its icon from `scry/crates/scry-rs-cli/assets/scry.res`, which `build.rs` hands to
the MSVC linker. It is compiled ahead of time, so building needs no resource compiler. To change
the icon:

1. Render [`assets/scry-rs-logo.svg`](../assets/scry-rs-logo.svg) at 16, 20, 24, 32, 40, 48, 64 and
   256 pixels into `scry.ico`.
2. Run `rc /fo scry.res scry.rc` (from the Windows SDK) in that `assets` folder.

## Releasing

1. Set the version in `scry/Cargo.toml` (`[workspace.package]`), and the `scry-rs-derive`
   dependency's version in `scry/crates/scry-rs/Cargo.toml` to match.
2. Publish the crates, in dependency order:
   ```sh
   cargo publish -p scry-rs-derive
   cargo publish -p scry-rs
   cargo publish -p scry-rs-cargo
   ```
3. Tag the commit `v<version>` and push the tag. [`release.yml`](../.github/workflows/release.yml)
   builds `scry-rs` on Windows, Linux and macOS (x64 and Arm64 each), checks that each build runs,
   and publishes the archives, with a `SHA256SUMS` file, as a GitHub release. The workflow checks that the tag matches the
   workspace version.

The Skill downloads `scry-rs` from the latest release, so a release is what updates the Skill's
users.
