//! The in-process endpoint: a loopback TCP listener that runs registry commands for the `scry` CLI.
//!
//! The model follows Scry.NET's: the process listens on `127.0.0.1` on a port the OS picks, and
//! writes a descriptor - pid, port and a random capability token - to a per-user directory
//! ([`targets_dir`]). A client must present the token with every request, so being able to reach
//! the port is not enough; being able to read the user's descriptor is what grants access.
//!
//! The protocol is one JSON object per line each way:
//!
//! ```text
//! -> {"token": "...", "command": "get APP.stats 2"}
//! <- {"ok": true, "result": {...}}
//! ```
//!
//! Commands run on the registry's home thread, never on the connection's: [`listen`] hands each
//! one to a dispatcher, which queues it for that thread and waits for the reply.

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// A command and where to send its JSON reply.
pub type Job = (String, mpsc::Sender<String>);

/// How long a request waits for the registry's thread before giving up, for when that thread is
/// busy - a UI thread in a long event handler, say.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest request line accepted.
const MAX_REQUEST: u64 = 1 << 20;

/// Where descriptors live: `SCRY_RS_TARGETS` if set, else `.scry-rs\targets` in the user's home
/// (`%USERPROFILE%` on Windows, `$HOME` elsewhere). Only the user (and administrators) can read it.
///
/// Not `%LOCALAPPDATA%`: Windows redirects `AppData` writes made by processes inside a packaged
/// (MSIX) app such as Claude Desktop, so an app started from there and a CLI started from a plain
/// terminal would see different folders. The rest of the profile is not redirected.
pub fn targets_dir() -> PathBuf {
    if let Some(dir) = env::var_os("SCRY_RS_TARGETS") {
        return PathBuf::from(dir);
    }
    let home = if cfg!(windows) { env::var_os("USERPROFILE") } else { env::var_os("HOME") };
    PathBuf::from(home.unwrap_or_else(|| ".".into())).join(".scry-rs").join("targets")
}

/// Starts listening and writes this process's descriptor. `dispatch` queues a job for the
/// registry's thread and returns false once nothing is taking jobs any more.
pub fn listen(dispatch: impl Fn(Job) -> bool + Send + Sync + 'static) -> io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let token = new_token()?;
    write_descriptor(port, &token)?;
    eprintln!("scry: listening on 127.0.0.1:{port} (pid {})", std::process::id());
    if cfg!(panic = "abort") {
        eprintln!("scry: this build aborts on panic, so a command that panics ends the process");
    }

    let dispatch = std::sync::Arc::new(dispatch);
    thread::Builder::new().name("scry-endpoint".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let dispatch = std::sync::Arc::clone(&dispatch);
            let token = token.clone();
            let _ =
                thread::Builder::new().name("scry-connection".into()).spawn(move || serve(stream, &token, &*dispatch));
        }
    })?;
    Ok(())
}

fn serve(stream: TcpStream, token: &str, dispatch: &dyn Fn(Job) -> bool) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match (&mut reader).take(MAX_REQUEST).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let (reply, authorized) = respond(&line, token, dispatch);
        if writeln!(writer, "{reply}").is_err() || !authorized {
            return;
        }
    }
}

fn respond(line: &str, token: &str, dispatch: &dyn Fn(Job) -> bool) -> (Value, bool) {
    let Ok(request) = serde_json::from_str::<Value>(line) else {
        return (failure("bad_request", "a request is one JSON object per line"), true);
    };
    let presented = request["token"].as_str().unwrap_or("");
    if !same(presented.as_bytes(), token.as_bytes()) {
        return (failure("unauthorized", "missing or wrong token"), false);
    }
    let Some(command) = request["command"].as_str() else {
        return (failure("bad_request", "the request has no \"command\" string"), true);
    };
    // Hook commands work on captured records only, so they are answered here: a long `hook wait`
    // never occupies the registry's thread, which in a Dioxus app is the UI thread.
    if let Some(result) = crate::hooks::command(command) {
        return (crate::command::reply(result, Vec::new()), true);
    }
    let (sender, receiver) = mpsc::channel();
    if !dispatch((command.to_owned(), sender)) {
        return (failure("stopped", "the registry is no longer running"), true);
    }
    match receiver.recv_timeout(REPLY_TIMEOUT) {
        Ok(text) => (serde_json::from_str(&text).unwrap_or(Value::String(text)), true),
        Err(_) => (failure("target_busy", "the registry's thread did not answer in time"), true),
    }
}

fn failure(code: &str, message: &str) -> Value {
    json!({ "ok": false, "error": { "code": code, "message": message } })
}

/// Compares in time independent of where the inputs differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_descriptor(port: u16, token: &str) -> io::Result<()> {
    let dir = targets_dir();
    fs::create_dir_all(&dir)?;
    let pid = std::process::id();
    let process = env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_secs());
    let descriptor = json!({
        "pid": pid,
        "process": process,
        "address": format!("127.0.0.1:{port}"),
        "token": token,
        "startedAt": started,
        "version": env!("CARGO_PKG_VERSION"),
    });
    // Written aside and renamed, so a client never reads half a descriptor.
    let path = dir.join(format!("{pid}.json"));
    let partial = dir.join(format!("{pid}.json.partial"));
    fs::write(&partial, descriptor.to_string())?;
    fs::rename(&partial, &path)
}

/// Starts the endpoint with a registry on its own thread. For apps whose roots are all statics
/// and thread-safe; a Dioxus app uses `dioxus_hooks::start_endpoint` instead.
pub fn start() {
    let (sender, receiver) = mpsc::channel::<Job>();
    let spawned = thread::Builder::new().name("scry-registry".into()).spawn(move || {
        let registry = crate::Registry::discovered();
        for (command, reply) in receiver {
            let _ = reply.send(crate::execute(&registry, &command).to_string());
        }
    });
    if spawned.is_err() {
        eprintln!("scry: could not start the registry thread");
        return;
    }
    let sender = std::sync::Mutex::new(sender);
    if let Err(error) = listen(move |job| sender.lock().map(|sender| sender.send(job).is_ok()).unwrap_or(false)) {
        eprintln!("scry: could not start the endpoint: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_tokens_fully() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }

    #[test]
    fn refuses_a_wrong_token_and_answers_a_right_one() {
        let dispatch = |(command, reply): Job| reply.send(format!("{{\"ok\":true,\"result\":\"{command}\"}}")).is_ok();
        let (reply, authorized) = respond(r#"{"token":"nope","command":"roots"}"#, "secret", &dispatch);
        assert!(!authorized);
        assert_eq!(reply["error"]["code"], "unauthorized");
        let (reply, _) = respond(r#"{"token":"secret","command":"roots"}"#, "secret", &dispatch);
        assert_eq!(reply["result"], "roots");
    }
}
