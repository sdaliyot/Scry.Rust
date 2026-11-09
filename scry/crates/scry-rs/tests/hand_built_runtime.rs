//! An app that builds its tokio runtime by hand after `main` starts, as a server often does: the
//! endpoint starts before the runtime exists, so the runtime is found from the app's own code, the
//! first time a function with a hook point runs inside it. A test binary of its own, because the
//! runtime is remembered once per process.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use scry::__private::HookSite;

/// What `cargo scry` compiles into each of the app's functions.
static SITE: HookSite = HookSite::new("app::handle_request", "async fn handle_request()");

#[test]
fn a_runtime_built_after_main_starts_is_found_from_the_apps_code() {
    // The endpoint starts at the top of `main`: no runtime yet.
    scry::__private::capture_runtime();

    let (ready, started) = mpsc::channel();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async move {
            // The app's code runs, its hook point first.
            let _ = SITE.enter(|_| {});
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
    });
    started.recv().unwrap();

    // A call from the registry's thread: tokio's timer needs the runtime to fire.
    let slept = scry::__private::block_on(async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        "woke"
    });
    assert_eq!(slept.unwrap(), "woke");
}
