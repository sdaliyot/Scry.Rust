use serde_json::{Value, json};

use crate::{Error, Registry};

const DEFAULT_DEPTH: usize = 3;

const HELP: &str = "roots | ls <path> | get <path> [depth] | set <path> <json> | help";

/// Runs one text command against `registry` and returns a JSON reply:
/// `{"ok":true,"result":...}` or `{"ok":false,"error":{"code":...,"message":...}}`.
///
/// Commands: `roots`, `ls <path>`, `get <path> [depth]` (default depth 3), `set <path> <json>`.
/// A stand-in for the endpoint protocol, so the registry can be driven by hand before one exists.
pub fn execute(registry: &Registry, line: &str) -> Value {
    match run(registry, line.trim()) {
        Ok(result) => json!({ "ok": true, "result": result }),
        Err(error) => json!({ "ok": false, "error": { "code": error.code(), "message": error.to_string() } }),
    }
}

fn run(registry: &Registry, line: &str) -> Result<Value, Error> {
    let (verb, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(verb, rest)| (verb, rest.trim()));
    match verb {
        "roots" => Ok(json!(
            registry.roots().into_iter().map(|root| json!({ "name": root.name, "type": root.type_name })).collect::<Vec<_>>()
        )),
        "ls" => registry.describe(required(rest, "a path")?),
        "get" => {
            let (path, depth) = rest.split_once(char::is_whitespace).map_or((rest, None), |(path, depth)| (path, Some(depth)));
            let depth = match depth {
                Some(depth) => depth.trim().parse().map_err(|_| Error::BadCommand(format!("'{depth}' is not a depth")))?,
                None => DEFAULT_DEPTH,
            };
            registry.get(required(path, "a path")?, depth)
        }
        "set" => {
            let (path, json) = rest
                .split_once(char::is_whitespace)
                .ok_or_else(|| Error::BadCommand("usage: set <path> <json>".into()))?;
            let value: Value =
                serde_json::from_str(json.trim()).map_err(|error| Error::BadCommand(format!("invalid JSON value: {error}")))?;
            registry.set(path, &value)?;
            registry.get(path, DEFAULT_DEPTH)
        }
        "help" | "" => Ok(Value::String(HELP.into())),
        other => Err(Error::BadCommand(format!("unknown command '{other}'; {HELP}"))),
    }
}

fn required<'a>(text: &'a str, what: &str) -> Result<&'a str, Error> {
    if text.is_empty() { Err(Error::BadCommand(format!("expected {what}"))) } else { Ok(text) }
}
