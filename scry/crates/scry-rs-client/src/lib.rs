//! Talk to running [Scry.Rust](https://github.com/sdaliyot/Scry.Rust) apps from Rust code, such as
//! an integration test: everything the `scry-rs` command line does, in-process, over one
//! connection per app instead of a process per command.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), scry_rs_client::Error> {
//! // The app was started by the test, built with `cargo scry`.
//! let mut app = scry_rs_client::find().process("my-app").connect_within(Duration::from_secs(120))?;
//! app.wait("APP.stats.ticks >= 1", Duration::from_secs(30))?;
//! let ticks: u64 = app.get_as("APP.stats.ticks")?;
//! app.set("APP.rules[0].percent", 50)?;
//! app.click("text=Save")?;
//! app.screenshot(Some("ul"))?.save("list.png")?;
//! # Ok(())
//! # }
//! ```
//!
//! An app built with `cargo scry` listens on `127.0.0.1` and writes a descriptor - its pid,
//! address and a capability token - to a per-user folder ([`targets_dir`]). This crate finds apps
//! through those descriptors and sends the token with each request. The token is never shown:
//! [`Target`]'s `Debug` and [`Target::summary`] leave it out.
//!
//! Every reply is either a result or an [`Error`] carrying the same codes the command line prints
//! (`no_target`, `no_such_member`, `assertion_failed`, ...), so a test can match on
//! [`Error::code`]. [`Client::run`] sends any command the command line takes, for the ones without
//! a method of their own (`ls`, `fns`, `hook ...`, `window <n> ...`).

use std::env;
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// The longest a reply can take by default: `hook wait` may take up to five minutes by design.
const REPLY_TIMEOUT: Duration = Duration::from_secs(330);

/// What failed: an error the app replied with, or one on this side (finding the app, the
/// connection, a reply that doesn't fit).
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    code: String,
    message: String,
    details: Value,
}

impl Error {
    /// A new error, for code built on this crate.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), details: Value::Null }
    }

    /// The error's code: one the app replies with (`no_such_member`, `assertion_failed`, ...), or
    /// one of this crate's: `no_target`, `ambiguous_target`, `connection_failed`, `bad_reply`,
    /// `cannot_convert` ([`Client::get_as`]), `wait_timeout` ([`Client::wait`]), `output_failed`
    /// ([`Screenshot::save`]).
    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// The whole error object the app replied with, or what a [`Client::wait`] that ran out of
    /// time saw last; `null` otherwise.
    pub fn details(&self) -> &Value {
        &self.details
    }

    fn from_reply(error: &Value) -> Self {
        Self {
            code: error["code"].as_str().unwrap_or("error").to_owned(),
            message: error["message"].as_str().unwrap_or("").to_owned(),
            details: error.clone(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where apps write their descriptors: `$SCRY_RS_TARGETS` if set, else `~/.scry-rs/targets`.
pub fn targets_dir() -> PathBuf {
    // Must match `scry::endpoint::targets_dir`.
    if let Some(dir) = env::var_os("SCRY_RS_TARGETS") {
        return PathBuf::from(dir);
    }
    let home = if cfg!(windows) { env::var_os("USERPROFILE") } else { env::var_os("HOME") };
    PathBuf::from(home.unwrap_or_else(|| ".".into())).join(".scry-rs").join("targets")
}

/// A running app, as its descriptor describes it.
#[derive(Clone)]
pub struct Target {
    descriptor: Value,
    address: SocketAddr,
}

impl Target {
    pub fn pid(&self) -> u64 {
        self.descriptor["pid"].as_u64().unwrap_or(0)
    }

    /// The executable's name, without `.exe`.
    pub fn process(&self) -> &str {
        self.descriptor["process"].as_str().unwrap_or("")
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The descriptor without its token: what `scry-rs list` shows.
    pub fn summary(&self) -> Value {
        let mut summary = self.descriptor.clone();
        if let Some(object) = summary.as_object_mut() {
            object.remove("token");
        }
        summary
    }

    /// Opens a connection to the app.
    pub fn connect(&self) -> Result<Client> {
        let token = self.descriptor["token"]
            .as_str()
            .ok_or_else(|| Error::new("bad_reply", "the app's descriptor has no token"))?
            .to_owned();
        let stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(2)).map_err(connection_failed)?;
        let reader = BufReader::new(stream.try_clone().map_err(connection_failed)?);
        Ok(Client { target: self.clone(), token, stream: Some((stream, reader)) })
    }
}

/// Without the token.
impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Target")
            .field("pid", &self.pid())
            .field("process", &self.process())
            .field("address", &self.address)
            .finish()
    }
}

/// The apps whose endpoint answers, by pid. A descriptor whose port refuses connections belongs to
/// an app that has exited, and is deleted.
pub fn targets() -> Vec<Target> {
    targets_in(&targets_dir())
}

/// [`targets`], from descriptors in `dir` rather than [`targets_dir`]: for apps started with
/// `SCRY_RS_TARGETS` set to `dir`, as a test may do to keep its apps apart.
pub fn targets_in(dir: &Path) -> Vec<Target> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
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

/// Which app to talk to: all of them until narrowed with [`Find::pid`] or [`Find::process`].
pub fn find() -> Find {
    Find::default()
}

/// A filter over the running apps. See [`find`].
#[derive(Debug, Clone, Default)]
pub struct Find {
    pid: Option<u64>,
    process: Option<String>,
    dir: Option<PathBuf>,
}

impl Find {
    pub fn pid(mut self, pid: u64) -> Self {
        self.pid = Some(pid);
        self
    }

    /// The executable's name, without `.exe`; case doesn't matter.
    pub fn process(mut self, name: impl Into<String>) -> Self {
        self.process = Some(name.into());
        self
    }

    /// Looks for descriptors in `dir` rather than [`targets_dir`]: for an app started with
    /// `SCRY_RS_TARGETS` set to `dir`.
    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    pub fn matches(&self, target: &Target) -> bool {
        self.pid.is_none_or(|pid| target.pid() == pid)
            && self.process.as_deref().is_none_or(|name| target.process().eq_ignore_ascii_case(name))
    }

    /// The matching apps running now.
    pub fn all(&self) -> Vec<Target> {
        self.all_within(Duration::ZERO)
    }

    /// The matching apps, once at least one is running or `timeout` has passed. An app is listed
    /// once it can answer: a Dioxus app once its window is up.
    pub fn all_within(&self, timeout: Duration) -> Vec<Target> {
        let deadline = Instant::now() + timeout;
        loop {
            let all = self.dir.as_deref().map_or_else(targets, targets_in);
            let found: Vec<Target> = all.into_iter().filter(|target| self.matches(target)).collect();
            if !found.is_empty() || Instant::now() >= deadline {
                return found;
            }
            thread::sleep(Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    /// The one matching app running now: `no_target` if there is none, `ambiguous_target` if
    /// there are several.
    pub fn one(&self) -> Result<Target> {
        self.one_within(Duration::ZERO)
    }

    /// The one matching app, waiting up to `timeout` for it to start.
    pub fn one_within(&self, timeout: Duration) -> Result<Target> {
        let mut found = self.all_within(timeout);
        match found.len() {
            1 => Ok(found.remove(0)),
            0 if timeout.is_zero() => Err(Error::new("no_target", "no matching app is running")),
            0 => Err(Error::new("no_target", format!("no matching app started within {timeout:?}"))),
            _ => {
                let pids: Vec<String> =
                    found.iter().map(|target| format!("{} ({})", target.pid(), target.process())).collect();
                Err(Error::new("ambiguous_target", format!("several apps match, pick one by pid: {}", pids.join(", "))))
            }
        }
    }

    /// Connects to the one matching app running now.
    pub fn connect(&self) -> Result<Client> {
        self.one()?.connect()
    }

    /// Connects to the one matching app, waiting up to `timeout` for it to start. A console app
    /// answers from the first line of its `main`, so follow this with a [`Client::wait`] for state
    /// `main` sets up rather than reading it straight away.
    pub fn connect_within(&self, timeout: Duration) -> Result<Client> {
        self.one_within(timeout)?.connect()
    }
}

/// A connection to one app. Commands run one at a time, in order; for commands in parallel, open
/// another with [`Target::connect`].
pub struct Client {
    target: Target,
    token: String,
    stream: Option<(TcpStream, BufReader<TcpStream>)>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").field("target", &self.target).finish_non_exhaustive()
    }
}

impl Client {
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Sends a command line, as the command line takes it, and returns the app's whole reply:
    /// `{"ok": true, "result": ...}` or `{"ok": false, "error": {...}}`. Fails only when the reply
    /// doesn't arrive. After such a failure, the next command opens a new connection.
    pub fn send(&mut self, command: &str) -> Result<Value> {
        self.send_within(command, REPLY_TIMEOUT)
    }

    fn send_within(&mut self, command: &str, timeout: Duration) -> Result<Value> {
        if self.stream.is_none() {
            let fresh = self.target.connect()?;
            self.stream = fresh.stream;
        }
        let (stream, reader) = self.stream.as_mut().expect("connected above");
        let outcome = (|| {
            stream.set_read_timeout(Some(timeout))?;
            writeln!(stream, "{}", json!({ "token": self.token, "command": command }))?;
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the app closed the connection"));
            }
            Ok(line)
        })();
        let line = outcome.map_err(|error| {
            self.stream = None;
            connection_failed(error)
        })?;
        serde_json::from_str(&line).map_err(|error| Error::new("bad_reply", format!("unreadable reply: {error}")))
    }

    /// Sends a command line and returns its result, or the app's error.
    pub fn run(&mut self, command: &str) -> Result<Value> {
        result(self.send(command)?)
    }

    /// The registry's roots.
    pub fn roots(&mut self) -> Result<Value> {
        self.run("roots")
    }

    /// The value at `path`, such as `APP.stats` or `APP.rules[0]`, to the default depth.
    pub fn get(&mut self, path: &str) -> Result<Value> {
        self.run(&format!("get {path}"))
    }

    /// The value at `path`, nested values shown down to `depth`.
    pub fn get_depth(&mut self, path: &str, depth: u32) -> Result<Value> {
        self.run(&format!("get {path} {depth}"))
    }

    /// The value at `path`, as a `T`. Fails with `cannot_convert` if it doesn't fit.
    pub fn get_as<T: DeserializeOwned>(&mut self, path: &str) -> Result<T> {
        let value = self.get(path)?;
        serde_json::from_value(value).map_err(|error| Error::new("cannot_convert", format!("{path}: {error}")))
    }

    /// Sets the value at `path`, and returns it as it now is.
    pub fn set(&mut self, path: &str, value: impl Into<Value>) -> Result<Value> {
        self.run(&format!("set {path} {}", value.into()))
    }

    /// Calls a function or method (`Inventory::total`, `APP.inventory.total_value_cents`) with
    /// JSON arguments, and returns what it returned.
    pub fn call(&mut self, function: &str, args: &[Value]) -> Result<Value> {
        let mut command = format!("call {function}");
        for arg in args {
            command.push(' ');
            command.push_str(&arg.to_string());
        }
        self.run(&command)
    }

    /// Runs a Rhai script in the app and returns its value.
    pub fn eval(&mut self, script: &str) -> Result<Value> {
        self.run(&format!("eval {script}"))
    }

    /// Evaluates a Rhai condition in the app until it is `true`. Fails with `wait_timeout` if it
    /// isn't within `timeout`, with what it saw last in [`Error::details`]. The app caps a wait
    /// at five minutes.
    pub fn wait(&mut self, condition: &str, timeout: Duration) -> Result<()> {
        let command = format!("wait timeout={}ms {condition}", timeout.as_millis());
        let reply = result(self.send_within(&command, timeout.max(REPLY_TIMEOUT) + Duration::from_secs(30))?)?;
        if reply["satisfied"] == true {
            return Ok(());
        }
        let last = if reply.get("lastError").is_some() {
            format!("last error {}", reply["lastError"])
        } else {
            format!("last value {}", reply["lastValue"])
        };
        Err(Error {
            code: "wait_timeout".into(),
            message: format!("`{condition}` was not true within {timeout:?}: {last}"),
            details: reply,
        })
    }

    /// Evaluates a Rhai condition once. Fails with `assertion_failed` if it isn't `true`.
    pub fn assert(&mut self, condition: &str) -> Result<()> {
        self.run(&format!("assert {condition}")).map(drop)
    }

    /// A Dioxus app: the visible text of the page, or of the element `selector` names (a CSS
    /// selector, or `text=Label`).
    pub fn text(&mut self, selector: Option<&str>) -> Result<String> {
        let reply = match selector {
            Some(selector) => self.run(&format!("text {}", quoted(selector)))?,
            None => self.run("text")?,
        };
        Ok(reply.as_str().map_or_else(|| reply.to_string(), str::to_owned))
    }

    /// A Dioxus app: clicks the element `selector` names, and returns what it clicked. The app's
    /// handler runs after this returns, so check its effect with [`Client::wait`].
    pub fn click(&mut self, selector: &str) -> Result<Value> {
        self.run(&format!("click {}", quoted(selector)))
    }

    /// A Dioxus app: puts `text` into the input, textarea or select `selector` names, as typing
    /// would.
    pub fn type_text(&mut self, selector: &str, text: &str) -> Result<Value> {
        self.run(&format!("type {} {}", quoted(selector), quoted(text)))
    }

    /// A Dioxus app: runs JavaScript in the page and returns its value.
    pub fn js(&mut self, code: &str) -> Result<Value> {
        self.run(&format!("js {code}"))
    }

    /// A Dioxus desktop app: the page, or the element `selector` names, as the webview renders it.
    pub fn screenshot(&mut self, selector: Option<&str>) -> Result<Screenshot> {
        let reply = match selector {
            Some(selector) => self.run(&format!("screenshot {}", quoted(selector)))?,
            None => self.run("screenshot")?,
        };
        Screenshot::from_result(&reply)
    }
}

/// A PNG, as [`Client::screenshot`] returns it.
#[derive(Debug, Clone)]
pub struct Screenshot {
    /// The PNG file's bytes.
    pub png: Vec<u8>,
    /// In device pixels.
    pub width: u32,
    pub height: u32,
    /// What was captured, for an element.
    pub element: Option<Value>,
}

impl Screenshot {
    /// Reads the result of a `screenshot` command: `{format, width, height, base64Data, element?}`.
    pub fn from_result(result: &Value) -> Result<Self> {
        let data = result["base64Data"]
            .as_str()
            .ok_or_else(|| Error::new("bad_reply", "the reply has no image (base64Data)"))?;
        let png = decode_base64(data).ok_or_else(|| Error::new("bad_reply", "the image is not valid base64"))?;
        let size = |key: &str| result[key].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
        Ok(Self { png, width: size("width"), height: size("height"), element: result.get("element").cloned() })
    }

    /// Writes the PNG to `path`.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        fs::write(path, &self.png)
            .map_err(|error| Error::new("output_failed", format!("cannot write {}: {error}", path.display())))
    }
}

fn result(reply: Value) -> Result<Value> {
    if reply["ok"] == true {
        let mut reply = reply;
        Ok(reply["result"].take())
    } else {
        Err(Error::from_reply(&reply["error"]))
    }
}

fn connection_failed(error: std::io::Error) -> Error {
    Error::new("connection_failed", error.to_string())
}

/// A selector or text as one JSON string, so spaces and quotes in it stay where they are.
fn quoted(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3);
    let (mut bits, mut count) = (0u32, 0);
    for c in text.bytes().filter(|c| *c != b'=') {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            bytes.push((bits >> count) as u8);
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_become_results_or_errors() {
        assert_eq!(result(json!({ "ok": true, "result": 3 })), Ok(json!(3)));
        let error =
            result(json!({ "ok": false, "error": { "code": "no_such_root", "message": "no root X" } })).unwrap_err();
        assert_eq!((error.code(), error.message()), ("no_such_root", "no root X"));
        assert_eq!(error.to_string(), "no_such_root: no root X");
    }

    #[test]
    fn selectors_are_sent_whole() {
        assert_eq!(quoted("text=Add task"), r#""text=Add task""#);
        assert_eq!(quoted(r#"a[title="x"]"#), r#""a[title=\"x\"]""#);
    }

    #[test]
    fn screenshots_are_decoded() {
        let shot =
            Screenshot::from_result(&json!({ "format": "png", "width": 2, "height": 1, "base64Data": "iVBORw0KGgo=" }))
                .unwrap();
        assert_eq!(shot.png, b"\x89PNG\r\n\x1a\n");
        assert_eq!((shot.width, shot.height, shot.element), (2, 1, None));
        assert_eq!(Screenshot::from_result(&json!({})).unwrap_err().code(), "bad_reply");
    }

    #[test]
    fn the_token_is_never_shown() {
        let target = Target {
            descriptor: json!({ "pid": 7, "process": "app", "address": "127.0.0.1:1", "token": "secret" }),
            address: "127.0.0.1:1".parse().unwrap(),
        };
        assert!(!format!("{target:?}").contains("secret"));
        assert!(!target.summary().to_string().contains("secret"));
    }
}
