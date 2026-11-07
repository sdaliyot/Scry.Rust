# Security policy

## Supported versions

Only the latest release is supported. There is no long-term support branch: fixes land on `main`
and ship in the next release.

## Reporting a vulnerability

Please report a suspected vulnerability privately, not in a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/sdaliyot/Scry.Rust/security/advisories/new):
open the repository's "Security" tab and choose "Report a vulnerability". Only you and the
maintainer can see the advisory, so the details stay private until a fix ships.

Please include:
- The Scry.Rust version (crate version or release tag), or the commit if you built from source.
- Your operating system, and whether the app is a Dioxus app.
- Steps to reproduce, and what you expected the trust boundary to prevent.

## What Scry.Rust is, for context

Scry.Rust is local developer and test tooling. It is not a network service. Reading private state,
changing it and calling app code inside an app built with `cargo scry` is what it is for, so that
alone isn't a vulnerability. [`docs/threat-model.md`](docs/threat-model.md) describes the boundary
it does keep: who can reach an endpoint, and how an app opts in.
