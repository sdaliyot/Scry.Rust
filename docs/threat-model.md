# Threat model

Scry.Rust lets a process outside an app read and change the app's state, call its functions, change
its calls through hooks, and run scripts and page JavaScript inside it. That is the product. This
document describes who may do that, how the app opts in, and what Scry.Rust doesn't defend against.

## What is protected

An app built with `cargo scry` runs an endpoint that can do anything the app's own code can do with
its state. Access to the endpoint is therefore code-execution-equivalent access to the app, running
as the app's user. The asset is that access, and the boundary is who gets it.

## How an app opts in

Nothing is injected into a running process. An app takes part only when **both** are true:

1. Its manifest has the optional `scry` dependency (`scry = { package = "scry-rs", optional = true }`)
   and a `scry` feature that turns it on.
2. It was built with `cargo scry`, which turns that feature on and generates the registry.

A plain `cargo build` of the same sources contains no Scry code and no endpoint, so the binaries an
app ships don't have one unless they were built with `cargo scry`. **Don't ship a `cargo scry`
build.** Debug or release makes no difference: a `cargo scry build --release` has the endpoint too.

## Who can reach the endpoint

- **Loopback only.** The endpoint listens on `127.0.0.1`, on a port the operating system picks. It
  can't be reached from another machine without a port forward that someone sets up deliberately.
  The `scry-rs` command line ignores a descriptor whose address isn't a loopback address.
- **A capability token on every request.** At startup the app generates 32 random bytes from the
  operating system's generator (256 bits). Every request must carry this token, which is compared in
  constant time. A connection that presents a wrong token gets one `unauthorized` reply and is
  closed.
- **The token's only copy is the descriptor.** The app writes the pid, the port and the token to
  `~/.scry-rs/targets/<pid>.json` (`%USERPROFILE%\.scry-rs\targets` on Windows), or to the folder
  that `SCRY_RS_TARGETS` names.
  - On Linux and macOS the file is created with mode `0600`, and the folder is made `0700` when the
    user can change it.
  - On Windows the file takes the profile folder's permissions, which allow only the user, SYSTEM
    and Administrators.
  - A folder chosen through `SCRY_RS_TARGETS` should be just as private, because whoever can read
    the descriptor can drive the app.

A loopback port has no per-user check of its own: any local process, running as any user, can open a
connection. The token is the gate, and the descriptor's file permissions are what keep it from other
users.

## What the endpoint lets a caller do

Everything below runs inside the app, as the app's user:

- read every registered static, `const` and tracked component state, including private fields
- write anything on a path that has a lock, an atomic or a signal
- call the app's functions and methods (except `unsafe` ones)
- run [Rhai](https://rhai.rs) scripts. They can't load modules, are stopped after 5 seconds, and
  have bounded string, array and map sizes. But they can do anything the registry can.
- add hooks that record calls, and hooks that change arguments and results
- in a Dioxus app with a webview, run JavaScript in the page, click and type, read the page's text
  and take screenshots

These are deliberately not restricted further. A caller who holds the token is trusted the way a
debugger attached to the process would be.

## What is not defended against

- **Another process running as the same user.** It can read the descriptor, and it could attach a
  debugger to the app anyway.
- **An administrator or root**, who can read any file and any process's memory.
- **A port forward that someone sets up.** Forwarding the endpoint's port to another machine exposes
  it there, with only the token in the way, and the descriptor then becomes a credential to carry
  across. Scry.Rust doesn't support that setup, and doesn't guard against it.
- **Code that is called.** A function called through Scry runs with the app's full rights, and
  so does a hook script. Scry catches panics so the app keeps running, but it can't make a called
  function harmless. A function that blocks holds the thread it runs on, which is the UI thread in
  a Dioxus app.
- **Denial of service by a token holder.** A caller with the token can keep the app busy, change
  its state into one it can't recover from, or make every call to a function fail. That is within
  what the token grants.

## Leftovers

The app doesn't delete its descriptor when it exits. The `scry-rs` command line deletes each
descriptor whose port no longer answers, the next time it lists targets. If a new process later
takes the same port, a stale descriptor's token won't match it, so the descriptor can't be used to
reach that process.

## Reporting

See [`SECURITY.md`](../SECURITY.md).
