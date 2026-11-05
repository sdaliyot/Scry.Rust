//! `wait` and `assert`: conditions on the app's state, as Rhai expressions.
//!
//! ```text
//! wait timeout=10s interval=100ms APP.stats.sold > 10
//! assert FILTER == "Done"
//! ```
//!
//! `wait` evaluates the condition until it gives `true` or the timeout passes. A timeout is not an
//! error: the reply says `satisfied: false`, with the last value or error, so a caller reads the
//! flag rather than the exit code. An error while waiting - a path that does not exist yet, a lock
//! that is busy - counts as "not yet". `assert` evaluates once and fails with `assertion_failed`
//! when the condition is not `true`. A condition that does not compile fails at once, for both.
//!
//! Each evaluation is an `eval` on the registry's thread. Through the endpoint, the waiting in
//! between happens on the connection's thread, so a long `wait` never holds the registry's thread
//! - a Dioxus app's UI thread - for longer than one evaluation.

use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Error;
use crate::command::reply;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);
const MAX_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) const HELP: &str = "wait [timeout=10s] [interval=100ms] <rhai condition> | assert <rhai condition>";

/// Runs `wait` or `assert` and returns the reply; `None` for any other command. `evaluate` runs
/// `eval <condition>` against the registry and returns its reply.
pub(crate) fn command(line: &str, evaluate: &mut dyn FnMut(&str) -> Value) -> Option<Value> {
    let line = line.trim();
    let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    match verb {
        "wait" => Some(wait(rest, evaluate).unwrap_or_else(|error| reply(Err(error), Vec::new()))),
        "assert" => Some(assert(rest, evaluate).unwrap_or_else(|error| reply(Err(error), Vec::new()))),
        _ => None,
    }
}

fn wait(text: &str, evaluate: &mut dyn FnMut(&str) -> Value) -> Result<Value, Error> {
    let (options, condition) = crate::hooks::options(text);
    let mut timeout = DEFAULT_TIMEOUT;
    let mut interval = DEFAULT_INTERVAL;
    for (key, value) in options {
        match key {
            "timeout" => timeout = crate::hooks::duration(key, value)?.min(MAX_TIMEOUT),
            "interval" => interval = crate::hooks::duration(key, value)?.max(Duration::from_millis(10)),
            _ => return Err(Error::BadCommand(format!("unknown option '{key}'; {HELP}"))),
        }
    }
    let condition = checked(condition)?;
    let started = Instant::now();
    let deadline = started + timeout;
    let mut attempts = 0u64;
    loop {
        attempts += 1;
        let outcome = evaluate(condition);
        let elapsed = started.elapsed().as_millis() as u64;
        if outcome["ok"] == true && outcome["result"] == true {
            return Ok(reply(Ok(json!({ "satisfied": true, "attempts": attempts, "elapsedMs": elapsed })), Vec::new()));
        }
        if Instant::now() + interval > deadline {
            let mut result = json!({ "satisfied": false, "attempts": attempts, "elapsedMs": elapsed });
            if outcome["ok"] == true {
                result["lastValue"] = outcome["result"].clone();
            } else {
                result["lastError"] = outcome["error"].clone();
            }
            return Ok(reply(Ok(result), Vec::new()));
        }
        thread::sleep(interval);
    }
}

fn assert(text: &str, evaluate: &mut dyn FnMut(&str) -> Value) -> Result<Value, Error> {
    let condition = checked(text.trim())?;
    let outcome = evaluate(condition);
    if outcome["ok"] != true {
        return Ok(outcome);
    }
    match &outcome["result"] {
        Value::Bool(true) => Ok(reply(Ok(json!({ "satisfied": true })), Vec::new())),
        other => Err(Error::AssertionFailed(format!("`{condition}` gave {other}, not true"))),
    }
}

/// The condition, if there is one and it compiles.
fn checked(condition: &str) -> Result<&str, Error> {
    if condition.is_empty() {
        return Err(Error::BadCommand(format!("expected a condition; {HELP}")));
    }
    #[cfg(feature = "script")]
    crate::script::check(condition)?;
    #[cfg(not(feature = "script"))]
    return Err(Error::BadCommand("conditions need scry-rs's `script` feature, which this build turned off".into()));
    #[cfg(feature = "script")]
    Ok(condition)
}
