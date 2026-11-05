//! An `async fn` called from a command: run inside the app's tokio runtime, from another thread.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[test]
fn async_calls_run_inside_the_apps_tokio_runtime() {
    // The app: a runtime whose thread stays inside it, as `#[tokio::main]` keeps `main` there.
    let (ready, started) = mpsc::channel();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async move {
            scry::__private::capture_runtime();
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
