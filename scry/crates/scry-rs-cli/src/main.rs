//! `scry`: talks to running processes built with `cargo scry`.
//!
//! ```text
//! scry-rs list                                  running targets, as JSON
//! scry-rs [--pid N | --process NAME] <command>  run a registry command in one of them
//! scry-rs --out shot.png screenshot [selector]   save a Dioxus app's screenshot as a PNG file
//! scry-rs --wait-for-target 2m <command>         wait for the app to start first
//! ```
//!
//! A command is what the registry understands - `roots`, `ls <path>`, `get <path> [depth]`,
//! `set <path> <json>`, `eval <rhai script>` - and its reply is printed as one line of JSON. The
//! exit code is 0 when the reply is `"ok": true`. With a single target running, `--pid` can be
//! left out. `eval --file PATH` and `eval -` read a longer script from a file or stdin.
//!
//! Targets are found, and commands sent, through `scry-rs-client`, which tests can use directly;
//! the token in a descriptor is sent with each request and never printed.

use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use scry_rs_client::{Find, Screenshot, Target};
use serde_json::{Value, json};

const USAGE: &str = "usage: scry-rs [--wait-for-target DURATION] list
       scry-rs [--pid N | --process NAME] [--wait-for-target DURATION] [--out FILE] <command>
       scry-rs --version

commands: roots | ls <path> | get <path> [depth] | set <path> <json> | help
          call <function or path.method> [json args...] | fns [filter]
          eval <rhai script> | eval --file <path> | eval -   (script from stdin)
          wait [timeout=10s] [interval=100ms] <rhai condition> | assert <rhai condition>
          hooks [filter] | hook add <function> [depth=2] [capacity=1000] [self=true] [ttl=30m]
          hook read <id> [cursor] | hook wait <id> [timeout=30s] [cursor=n] [<rhai predicate>]
          hook before <id> [<rhai>] | hook after <id> [<rhai>]   (change calls; empty removes)
          hook remove <id> | hook clear
          js <code> | click <selector> | type <selector> <text> | text [selector]
                                       (a Dioxus app's page; selector: CSS, or text=Label)
          screenshot [selector]          (a Dioxus desktop app's page, as a PNG; --out FILE saves it)
          windows | window <n> <command>   (a Dioxus app's other windows)

--wait-for-target waits up to DURATION (500ms, 30s, 2m; a bare number is seconds) for a matching
app to start, instead of failing with no_target at once.";

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let mut pid = None;
    let mut process = None;
    let mut out = None;
    let mut wait_for_target = Duration::ZERO;
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--pid" if args.len() > 1 => {
                pid = args[1].parse::<u64>().ok();
                if pid.is_none() {
                    return fail("usage", &format!("'{}' is not a pid", args[1]));
                }
                args.drain(..2);
            }
            "--process" if args.len() > 1 => {
                process = Some(args[1].clone());
                args.drain(..2);
            }
            "--out" if args.len() > 1 => {
                out = Some(PathBuf::from(&args[1]));
                args.drain(..2);
            }
            "--wait-for-target" if args.len() > 1 => {
                match parse_duration(&args[1]) {
                    Some(duration) => wait_for_target = duration,
                    None => return fail("usage", &format!("'{}' is not a duration, such as 30s or 2m", args[1])),
                }
                args.drain(..2);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("{}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ => break,
        }
    }
    if args.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    }
    let mut find = Find::default();
    if let Some(pid) = pid {
        find = find.pid(pid);
    }
    if let Some(process) = process {
        find = find.process(process);
    }
    if args[0] == "list" {
        let list: Vec<Value> = find.all_within(wait_for_target).iter().map(Target::summary).collect();
        println!("{}", Value::Array(list));
        return ExitCode::SUCCESS;
    }

    let command = match command(&args) {
        Ok(command) => command,
        Err(error) => return fail("usage", &error),
    };
    let candidates = find.all_within(wait_for_target);
    let target = match candidates.as_slice() {
        [target] => target,
        [] if wait_for_target.is_zero() => return fail("no_target", "no matching running target; see `scry-rs list`"),
        [] => {
            return fail(
                "no_target",
                &format!("no matching target started within {}; see `scry-rs list`", show_duration(wait_for_target)),
            );
        }
        many => {
            let pids: Vec<String> =
                many.iter().map(|target| format!("{} ({})", target.pid(), target.process())).collect();
            return fail(
                "ambiguous_target",
                &format!("several targets match, pick one with --pid: {}", pids.join(", ")),
            );
        }
    };

    let reply = target.connect().and_then(|mut client| client.send(&command));
    match reply {
        Ok(mut reply) => {
            if let Some(out) = &out
                && let Err(error) = save_image(&mut reply, out)
            {
                return fail("output_failed", &error);
            }
            println!("{reply}");
            if reply["ok"] == true { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(error) => fail(error.code(), error.message()),
    }
}

/// `500ms`, `30s`, `2m`, or a bare number of seconds.
fn parse_duration(text: &str) -> Option<Duration> {
    let (number, unit) = text.find(|c: char| !c.is_ascii_digit()).map_or((text, ""), |at| text.split_at(at));
    let number: u64 = number.parse().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(number)),
        "" | "s" => Some(Duration::from_secs(number)),
        "m" => Some(Duration::from_secs(number.checked_mul(60)?)),
        _ => None,
    }
}

fn show_duration(duration: Duration) -> String {
    match duration.as_millis() {
        ms if ms % 60_000 == 0 => format!("{}m", ms / 60_000),
        ms if ms % 1000 == 0 => format!("{}s", ms / 1000),
        ms => format!("{ms}ms"),
    }
}

/// The command to send: the words as given, except that `eval --file PATH` and `eval -` take the
/// script from a file or stdin.
fn command(args: &[String]) -> Result<String, String> {
    let script = match args {
        [verb, flag, path] if verb == "eval" && flag == "--file" => {
            fs::read_to_string(path).map_err(|error| format!("cannot read {path}: {error}"))?
        }
        [verb, dash] if verb == "eval" && dash == "-" => {
            let mut script = String::new();
            io::stdin().read_to_string(&mut script).map_err(|error| format!("cannot read stdin: {error}"))?;
            script
        }
        _ => return Ok(args.join(" ")),
    };
    Ok(format!("eval {script}"))
}

/// `--out`: writes the reply's image (`base64Data`, as `screenshot` answers) to `path`, and puts
/// the file's path in the reply in its place.
fn save_image(reply: &mut Value, path: &PathBuf) -> Result<(), String> {
    if reply["ok"] != true {
        return Ok(());
    }
    let result = &mut reply["result"];
    if result.get("base64Data").is_none() {
        return Err("--out saves an image, and the reply has none".into());
    }
    let image = Screenshot::from_result(result).map_err(|error| error.message().to_owned())?;
    image.save(path).map_err(|error| error.message().to_owned())?;
    let bytes = image.png;
    let saved = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
    // Without Windows' verbatim prefix (`\\?\C:\...`), which canonicalize adds.
    let saved = saved.display().to_string();
    let saved = saved.strip_prefix(r"\\?\").unwrap_or(&saved);
    if let Some(result) = result.as_object_mut() {
        result.remove("base64Data");
        result.insert("file".into(), Value::String(saved.to_owned()));
        result.insert("bytes".into(), Value::from(bytes.len()));
    }
    Ok(())
}

fn fail(code: &str, message: &str) -> ExitCode {
    println!("{}", json!({ "ok": false, "error": { "code": code, "message": message } }));
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("2h"), None);
        assert_eq!(parse_duration("s"), None);
        assert_eq!(parse_duration("-1"), None);
        assert_eq!(show_duration(Duration::from_secs(120)), "2m");
        assert_eq!(show_duration(Duration::from_secs(90)), "90s");
        assert_eq!(show_duration(Duration::from_millis(1500)), "1500ms");
    }
}
