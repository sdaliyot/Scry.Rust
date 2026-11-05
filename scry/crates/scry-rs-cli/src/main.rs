//! `scry`: talks to running processes built with `cargo scry`.
//!
//! ```text
//! scry list                                  running targets, as JSON
//! scry [--pid N | --process NAME] <command>  run a registry command in one of them
//! ```
//!
//! A command is what the registry understands - `roots`, `ls <path>`, `get <path> [depth]`,
//! `set <path> <json>`, `eval <rhai script>` - and its reply is printed as one line of JSON. The
//! exit code is 0 when the reply is `"ok": true`. With a single target running, `--pid` can be
//! left out. `eval --file PATH` and `eval -` read a longer script from a file or stdin.
//!
//! Targets are found through the descriptors they write (see `scry::endpoint`); the token in a
//! descriptor is sent with each request and never printed.

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

const USAGE: &str = "usage: scry list
       scry [--pid N | --process NAME] <command>

commands: roots | ls <path> | get <path> [depth] | set <path> <json> | help
          call <function or path.method> [json args...] | fns [filter]
          eval <rhai script> | eval --file <path> | eval -   (script from stdin)
          wait [timeout=10s] [interval=100ms] <rhai condition> | assert <rhai condition>
          hooks [filter] | hook add <function> [depth=2] [capacity=1000] [self=true] [ttl=30m]
          hook read <id> [cursor] | hook wait <id> [timeout=30s] [cursor=n] [<rhai predicate>]
          hook before <id> [<rhai>] | hook after <id> [<rhai>]   (change calls; empty removes)
          hook remove <id> | hook clear";

/// Must match `scry::endpoint::targets_dir`.
fn targets_dir() -> PathBuf {
    if let Some(dir) = env::var_os("SCRY_RS_TARGETS") {
        return PathBuf::from(dir);
    }
    let home = if cfg!(windows) { env::var_os("USERPROFILE") } else { env::var_os("HOME") };
    PathBuf::from(home.unwrap_or_else(|| ".".into())).join(".scry-rs").join("targets")
}

struct Target {
    descriptor: Value,
    address: SocketAddr,
}

impl Target {
    fn pid(&self) -> u64 {
        self.descriptor["pid"].as_u64().unwrap_or(0)
    }

    fn process(&self) -> &str {
        self.descriptor["process"].as_str().unwrap_or("")
    }

    /// What `list` shows: everything but the token.
    fn summary(&self) -> Value {
        let mut summary = self.descriptor.clone();
        if let Some(object) = summary.as_object_mut() {
            object.remove("token");
        }
        summary
    }
}

/// Targets whose endpoint still answers. A descriptor whose port refuses connections belongs to
/// a process that has exited, and is deleted.
fn targets() -> Vec<Target> {
    let Ok(entries) = fs::read_dir(targets_dir()) else { return Vec::new() };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(descriptor) =
            fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            continue;
        };
        let Some(address) = descriptor["address"].as_str().and_then(|address| address.parse::<SocketAddr>().ok())
        else {
            continue;
        };
        if !address.ip().is_loopback() {
            continue;
        }
        if TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok() {
            found.push(Target { descriptor, address });
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    found.sort_by_key(Target::pid);
    found
}

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let mut pid = None;
    let mut process = None;
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
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => break,
        }
    }
    if args.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    }
    if args[0] == "list" {
        let list: Vec<Value> = targets().iter().map(Target::summary).collect();
        println!("{}", Value::Array(list));
        return ExitCode::SUCCESS;
    }

    let command = match command(&args) {
        Ok(command) => command,
        Err(error) => return fail("usage", &error),
    };
    let candidates: Vec<Target> = targets()
        .into_iter()
        .filter(|target| pid.is_none_or(|pid| target.pid() == pid))
        .filter(|target| process.as_deref().is_none_or(|name| target.process().eq_ignore_ascii_case(name)))
        .collect();
    let target = match candidates.as_slice() {
        [target] => target,
        [] => return fail("no_target", "no matching running target; see `scry list`"),
        many => {
            let pids: Vec<String> =
                many.iter().map(|target| format!("{} ({})", target.pid(), target.process())).collect();
            return fail(
                "ambiguous_target",
                &format!("several targets match, pick one with --pid: {}", pids.join(", ")),
            );
        }
    };

    match send(target, &command) {
        Ok(reply) => {
            println!("{reply}");
            if reply["ok"] == true { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(error) => fail("connection_failed", &error),
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

fn send(target: &Target, command: &str) -> Result<Value, String> {
    let token = target.descriptor["token"].as_str().ok_or("the descriptor has no token")?;
    let mut stream =
        TcpStream::connect_timeout(&target.address, Duration::from_secs(2)).map_err(|error| error.to_string())?;
    // `hook wait` may take up to five minutes by design.
    stream.set_read_timeout(Some(Duration::from_secs(330))).map_err(|error| error.to_string())?;
    writeln!(stream, "{}", json!({ "token": token, "command": command })).map_err(|error| error.to_string())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|error| error.to_string())?;
    serde_json::from_str(&line).map_err(|error| format!("unreadable reply: {error}"))
}

fn fail(code: &str, message: &str) -> ExitCode {
    println!("{}", json!({ "ok": false, "error": { "code": code, "message": message } }));
    ExitCode::FAILURE
}
