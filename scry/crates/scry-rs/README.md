# scry-rs

The runtime of [Scry.Rust](https://github.com/sdaliyot/Scry.Rust): reflection over a running Rust
app's state, and the local endpoint that the `scry-rs` command line and AI agents talk to.

An app doesn't use this crate directly. It adds it as an optional dependency named `scry`, and
[`cargo scry`](https://crates.io/crates/scry-rs-cargo) builds the app with everything it needs
generated in:

```toml
[features]
scry = ["dep:scry"]

[dependencies]
scry = { package = "scry-rs", version = "0.1", optional = true }
```

A plain `cargo build` doesn't include it. See the
[repository's README](https://github.com/sdaliyot/Scry.Rust#readme) for what it can do.
