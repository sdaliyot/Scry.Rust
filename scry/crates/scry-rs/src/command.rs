use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::{Value, json};

use crate::{Error, Registry};

pub(crate) const DEFAULT_DEPTH: usize = 3;

/// How deep a function's result is snapshotted: results are usually small, and often the point.
pub(crate) const RESULT_DEPTH: usize = 8;

const HELP: &str = "roots | ls <path> | get <path> [depth] | set <path> <json> 
                    | call <function or path.method> [json args] | fns [filter] | eval <rhai script> | help";

/// Runs one text command against `registry` and returns a JSON reply:
/// `{"ok":true,"result":...}` or `{"ok":false,"error":{"code":...,"message":...}}`, plus an
/// `"output"` array with whatever a script printed.
///
/// Commands: `roots`, `ls <path>`, `get <path> [depth]` (default depth 3), `set <path> <json>`,
/// `call <target> [args]` (see [`Registry::call`]; arguments are JSON values separated by
/// spaces), `fns [filter]`, and with the `script` feature `eval <rhai script>` (see the `script`
/// module docs).
///
/// A panic anywhere in a command - app code it calls, a `Debug` impl it reads through - is caught
/// and returned as a `panicked` error, and a lock held at the time is left unpoisoned (see
/// `impls::Held`), so the app carries on as if nothing happened. Only a build with
/// `panic = "abort"`, or an app panic hook that exits, still ends the process.
pub fn execute(registry: &Registry, line: &str) -> Value {
    let mut output = Vec::new();
    let result = catch_unwind(AssertUnwindSafe(|| run(registry, line.trim(), &mut output)))
        .unwrap_or_else(|panic| Err(Error::Panicked(crate::error::panic_message(&*panic))));
    let mut reply = match result {
        Ok(result) => json!({ "ok": true, "result": result }),
        Err(error) => json!({ "ok": false, "error": { "code": error.code(), "message": error.to_string() } }),
    };
    if !output.is_empty() {
        reply["output"] = json!(output);
    }
    reply
}

fn run(registry: &Registry, line: &str, output: &mut Vec<String>) -> Result<Value, Error> {
    let (verb, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(verb, rest)| (verb, rest.trim()));
    match verb {
        "roots" => Ok(json!(
            registry
                .roots()
                .into_iter()
                .map(|root| json!({ "name": root.name, "type": crate::reflect::short_type_name(root.type_name) }))
                .collect::<Vec<_>>()
        )),
        "ls" => registry.describe(required(rest, "a path")?),
        "get" => {
            let (path, depth) =
                rest.split_once(char::is_whitespace).map_or((rest, None), |(path, depth)| (path, Some(depth)));
            let depth = match depth {
                Some(depth) => {
                    depth.trim().parse().map_err(|_| Error::BadCommand(format!("'{depth}' is not a depth")))?
                }
                None => DEFAULT_DEPTH,
            };
            registry.get(required(path, "a path")?, depth)
        }
        "set" => {
            let (path, json) = rest
                .split_once(char::is_whitespace)
                .ok_or_else(|| Error::BadCommand("usage: set <path> <json>".into()))?;
            // Text that is not JSON is taken as a string, so `set FILTER Done` works without the
            // quotes that shells - Windows PowerShell especially - tend to strip.
            let json = json.trim();
            let value: Value = serde_json::from_str(json).unwrap_or_else(|_| Value::String(json.to_owned()));
            registry.set(path, &value)?;
            registry.get(path, DEFAULT_DEPTH)
        }
        "call" => {
            let (target, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            registry.call(required(target, "a function or method")?, &arguments(args), RESULT_DEPTH)
        }
        "fns" => Ok(crate::call::list(rest)),
        #[cfg(feature = "script")]
        "eval" => crate::script::evaluate(registry, required(rest, "a script")?, DEFAULT_DEPTH, output),
        #[cfg(not(feature = "script"))]
        "eval" => {
            let _ = output;
            Err(Error::BadCommand("scripts need scry-rs's `script` feature, which this build turned off".into()))
        }
        "help" | "" => Ok(Value::String(HELP.into())),
        other => Err(Error::BadCommand(format!("unknown command '{other}'; {HELP}"))),
    }
}

/// JSON values separated by whitespace. Text that does not parse as JSON is split on whitespace
/// instead, and each word that is not JSON taken as a string, as `set` does.
fn arguments(text: &str) -> Vec<Value> {
    let parsed: Result<Vec<Value>, _> = serde_json::Deserializer::from_str(text).into_iter::<Value>().collect();
    parsed.unwrap_or_else(|_| {
        text.split_whitespace()
            .map(|word| serde_json::from_str(word).unwrap_or_else(|_| Value::String(word.to_owned())))
            .collect()
    })
}

fn required<'a>(text: &'a str, what: &str) -> Result<&'a str, Error> {
    if text.is_empty() { Err(Error::BadCommand(format!("expected {what}"))) } else { Ok(text) }
}
