use serde_json::{Value, json};

use crate::{Error, Registry};

pub(crate) const DEFAULT_DEPTH: usize = 3;

const HELP: &str = "roots | ls <path> | get <path> [depth] | set <path> <json> | eval <rhai script> | help";

/// Runs one text command against `registry` and returns a JSON reply:
/// `{"ok":true,"result":...}` or `{"ok":false,"error":{"code":...,"message":...}}`, plus an
/// `"output"` array with whatever a script printed.
///
/// Commands: `roots`, `ls <path>`, `get <path> [depth]` (default depth 3), `set <path> <json>`,
/// and with the `script` feature `eval <rhai script>` (see the `script` module docs).
pub fn execute(registry: &Registry, line: &str) -> Value {
    let mut output = Vec::new();
    let mut reply = match run(registry, line.trim(), &mut output) {
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
            // Text that is not JSON is taken as a string, so `set FILTER Done` works without the
            // quotes that shells - Windows PowerShell especially - tend to strip.
            let json = json.trim();
            let value: Value = serde_json::from_str(json).unwrap_or_else(|_| Value::String(json.to_owned()));
            registry.set(path, &value)?;
            registry.get(path, DEFAULT_DEPTH)
        }
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

fn required<'a>(text: &'a str, what: &str) -> Result<&'a str, Error> {
    if text.is_empty() { Err(Error::BadCommand(format!("expected {what}"))) } else { Ok(text) }
}
