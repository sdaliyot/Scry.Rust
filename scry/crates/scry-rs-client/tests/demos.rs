//! Drives the demos (`demos/`, built with `cargo scry`) through the client, the way an app's own
//! tests would. They need the built demos, so they run only with `SCRY_RS_DEMOS` set to the folder
//! that holds them; `scripts/e2e.sh` does that:
//!
//! ```text
//! SCRY_RS_DEMOS=demos/target/scry/debug cargo test -p scry-rs-client --test demos
//! ```

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use scry_rs_client::{Client, find};
use serde_json::json;

/// A demo started for one test, with a descriptor folder of its own; stopped when dropped.
struct App {
    child: Child,
    dir: PathBuf,
}

impl App {
    /// `None` (and the test does nothing) when `SCRY_RS_DEMOS` isn't set.
    fn start(name: &str) -> Option<Self> {
        let Some(demos) = env::var_os("SCRY_RS_DEMOS") else {
            eprintln!("skipped: set SCRY_RS_DEMOS to the folder with the demos built by `cargo scry`");
            return None;
        };
        let dir = env::temp_dir().join(format!("scry-rs-client-test-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let program = PathBuf::from(demos).join(format!("{name}{}", env::consts::EXE_SUFFIX));
        let child = Command::new(&program)
            .env("SCRY_RS_TARGETS", &dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("cannot start {}: {error}", program.display()));
        Some(Self { child, dir })
    }

    fn connect(&self, name: &str, timeout: Duration) -> Client {
        find().in_dir(&self.dir).process(name).connect_within(timeout).unwrap()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn console_demo() {
    let Some(app) = App::start("console-demo") else { return };
    let mut client = app.connect("console-demo", Duration::from_secs(60));
    assert_eq!(client.target().process(), "console-demo");

    // The endpoint starts on main's first line: wait for the state main sets up.
    client.wait("APP.stats.ticks >= 1", Duration::from_secs(30)).unwrap();
    assert!(client.roots().unwrap().to_string().contains("console_demo::APP"));

    client.set("APP.rules[0].percent", 50).unwrap();
    assert!(client.get("APP.rules[0]").unwrap().to_string().contains(r#""percent":50"#));
    assert!(client.call("APP.inventory.total_value_cents", &[]).unwrap().is_number());
    assert_eq!(client.eval("1 + 2").unwrap(), json!(3));
    let ticks: u64 = client.get_as("APP.stats.ticks").unwrap();
    assert!(ticks >= 1);
    assert_eq!(client.get_as::<String>("APP.stats.ticks").unwrap_err().code(), "cannot_convert");

    assert_eq!(client.assert("APP.stats.ticks < 0").unwrap_err().code(), "assertion_failed");
    let timeout = client.wait("APP.stats.ticks < 0", Duration::from_millis(300)).unwrap_err();
    assert_eq!(timeout.code(), "wait_timeout");
    assert_eq!(timeout.details()["lastValue"], json!(false));
    assert!(client.get("APP.no_such_field").is_err());
    // An app's error leaves the connection usable.
    assert_eq!(client.eval("40 + 2").unwrap(), json!(42));

    let missing = find().in_dir(&app.dir).process("no-such-app").connect().unwrap_err();
    assert_eq!(missing.code(), "no_target");
}

#[test]
fn dx_demo() {
    let Some(app) = App::start("dx-demo") else { return };
    // An app is listed once its window is up, which takes a while under Xvfb.
    let mut client = app.connect("dx-demo", Duration::from_secs(180));

    client.set("FILTER", "Done").unwrap();
    assert!(client.text(Some("ul")).unwrap().contains("Install Rust"));
    assert_eq!(client.click("text=All").unwrap()["tag"], json!("button"));
    // The click runs the app's handler after it replies.
    client.wait(r#"FILTER == "All""#, Duration::from_secs(10)).unwrap();

    let shot = client.screenshot(Some("ul")).unwrap();
    assert!(shot.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(shot.width > 0 && shot.height > 0);
    assert_eq!(shot.element.as_ref().unwrap()["tag"], json!("ul"));
}
