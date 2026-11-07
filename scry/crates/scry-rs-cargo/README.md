# scry-rs-cargo

`cargo scry`, the build command of [Scry.Rust](https://github.com/sdaliyot/Scry.Rust). It builds a
crate with a registry generated for all of its code: its statics, types, functions and, in a Dioxus
app, its component state. It doesn't change the crate's sources.

```sh
cargo install scry-rs-cargo
cargo scry run            # or build, test, clippy, ...
cargo scry dx serve       # a Dioxus app built with the dx CLI
```

The app opts in with an optional [`scry-rs`](https://crates.io/crates/scry-rs) dependency. See the
[repository's README](https://github.com/sdaliyot/Scry.Rust#readme) for setup and usage.
