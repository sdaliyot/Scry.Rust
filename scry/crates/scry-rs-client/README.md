# scry-rs-client

Talk to running [Scry.Rust](https://github.com/sdaliyot/Scry.Rust) apps from Rust code, such as
an app's integration tests. It does everything the `scry-rs` command line does, in-process, over
one connection per app instead of a process per command.

```toml
[dev-dependencies]
scry-rs-client = "0.1"
```

```rust
use std::time::Duration;

// The app was started by the test, built with `cargo scry`.
let mut app = scry_rs_client::find().process("my-app").connect_within(Duration::from_secs(120))?;
app.wait("APP.stats.ticks >= 1", Duration::from_secs(30))?;
let ticks: u64 = app.get_as("APP.stats.ticks")?;
app.set("APP.rules[0].percent", 50)?;
app.click("text=Save")?;
app.screenshot(Some("ul"))?.save("list.png")?;
```

Errors carry the same codes the command line prints (`no_target`, `no_such_member`,
`assertion_failed`, ...), so a test can match on `error.code()`. `Client::run` sends any command
the command line takes.

See the [repository's README](https://github.com/sdaliyot/Scry.Rust#for-automated-tests) for how
an app opts in, and what the commands do.
