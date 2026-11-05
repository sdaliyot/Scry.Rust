//! Calling an `async fn` from a command: its future is run to completion on the calling thread.
//!
//! A call comes in on the registry's thread, which runs no executor, so the shim hands the future
//! to [`block_on`]: a minimal executor that polls it and parks the thread between wakeups. For an
//! app on tokio, the future runs inside the app's own runtime - remembered by the endpoint when it
//! starts at the top of `main`, which `#[tokio::main]` already runs inside - so tokio's timers,
//! I/O and `spawn` work as they do in the app. An app that builds its runtime by hand after `main`
//! starts is not seen; a future that needs tokio then panics, which the call reports.
//!
//! A call that has not finished after [`CALL_TIMEOUT`] fails with `timeout`, and its future is
//! dropped - cancelled at the `.await` it was waiting at, as a dropped future always is. The
//! registry's thread waits meanwhile; in a Dioxus app that is the UI thread, so a future that waits
//! for the UI (a signal, a Dioxus task) never completes and ends in that timeout.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use crate::Error;

/// How long an `async fn` call may run: less than the endpoint waits for the registry's thread, so
/// the caller gets this `timeout` rather than `target_busy`, and the thread is free again for the
/// next command.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(8);

const _: () = assert!(CALL_TIMEOUT.as_millis() < crate::endpoint::REPLY_TIMEOUT.as_millis());

#[cfg(feature = "tokio")]
static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// Remembers the tokio runtime the calling code runs in, if any, for [`block_on`].
pub fn capture_runtime() {
    #[cfg(feature = "tokio")]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let _ = RUNTIME.set(handle);
    }
}

struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` to completion on this thread, within the app's tokio runtime if it has one, or
/// fails with `timeout` after 8 seconds.
pub fn block_on<F: Future>(future: F) -> Result<F::Output, Error> {
    block_on_within(future, CALL_TIMEOUT)
}

fn block_on_within<F: Future>(future: F, timeout: Duration) -> Result<F::Output, Error> {
    #[cfg(feature = "tokio")]
    let _runtime = RUNTIME.get().map(tokio::runtime::Handle::enter);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    let deadline = Instant::now() + timeout;
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Ok(output);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::Timeout(format!(
                "the async call did not finish within {}s and was cancelled",
                timeout.as_secs_f32()
            )));
        }
        thread::park_timeout(deadline - now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_future_that_is_woken_from_another_thread() {
        let (sender, receiver) = futures_oneshot();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            sender(7);
        });
        assert_eq!(block_on(receiver).unwrap(), 7);
        assert_eq!(block_on(async { 1 + 1 }).unwrap(), 2);
    }

    /// Sets a flag when dropped, to tell that a timed-out future was cancelled.
    struct Dropped(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A future that never completes, holding `Dropped`.
    fn never(dropped: &Arc<std::sync::atomic::AtomicBool>) -> impl Future<Output = ()> {
        let guard = Dropped(Arc::clone(dropped));
        async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        }
    }

    #[test]
    fn a_call_that_does_not_finish_times_out_and_is_cancelled() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = Instant::now();
        let error = block_on_within(never(&dropped), Duration::from_millis(100)).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error.code(), "timeout");
        assert!(error.to_string().contains("did not finish within 0.1s and was cancelled"), "{error}");
        assert!(elapsed >= Duration::from_millis(100) && elapsed < Duration::from_secs(2), "{elapsed:?}");
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst), "the future is dropped, which cancels it");
    }

    /// The real limit, end to end. Takes 8 seconds: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn the_real_limit_is_eight_seconds() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = Instant::now();
        let error = block_on(never(&dropped)).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error.code(), "timeout");
        assert!(error.to_string().contains("within 8s"), "{error}");
        assert!(elapsed >= CALL_TIMEOUT && elapsed < CALL_TIMEOUT + Duration::from_secs(2), "{elapsed:?}");
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A one-shot channel as a future, so the test needs no executor crate.
    fn futures_oneshot() -> (impl FnOnce(u32) + Send, impl Future<Output = u32>) {
        use std::sync::Mutex;
        let shared = Arc::new(Mutex::new((None::<u32>, None::<Waker>)));
        let sender = {
            let shared = Arc::clone(&shared);
            move |value| {
                let mut state = shared.lock().unwrap();
                state.0 = Some(value);
                if let Some(waker) = state.1.take() {
                    waker.wake();
                }
            }
        };
        let receiver = std::future::poll_fn(move |context| {
            let mut state = shared.lock().unwrap();
            match state.0.take() {
                Some(value) => Poll::Ready(value),
                None => {
                    state.1 = Some(context.waker().clone());
                    Poll::Pending
                }
            }
        });
        (sender, receiver)
    }
}
