//! `cargo scry`: builds a crate with a Scry.Rust registry generated for all of its code.
//!
//! ```text
//! cargo scry run -p console-demo
//! cargo scry build --release
//! ```
//!
//! runs the cargo command with `--features scry`, and with this same binary installed as the
//! workspace's rustc wrapper. For each workspace crate that has the `scry` feature on and depends
//! on `scry`, the wrapper compiles a rewritten copy of its sources instead of the originals; every
//! other crate, and every build without `cargo scry`, compiles untouched. See [`rewrite`] for what
//! the copy adds.
//!
//! The rewritten copy is kept under `target/scry/<profile>/scry-src/<crate>-<hash>/`, next to a
//! `scry-report.txt` listing what was generated and what was skipped.
//!
//! Builds go to `target/scry`, so switching between `cargo build` and `cargo scry build` does not
//! rebuild the dependencies each time.

mod edits;
mod functions;
mod generate;
mod rewrite;
mod tokens;
mod wrapper;

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

/// Lets cargo tell whether a crate was rewritten by this build of the generator; see
/// [`wrapper::GENERATOR_ID`].
fn generator_id() -> String {
    let modified = env::current_exe()
        .and_then(std::fs::metadata)
        .map(|meta| {
            let time = meta.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
            format!("{}-{}", meta.len(), time.map_or(0, |time| time.as_secs()))
        })
        .unwrap_or_default();
    format!("{}-{modified}", env!("CARGO_PKG_VERSION"))
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    // cargo runs `cargo-scry scry <args>` for `cargo scry <args>`, and `cargo-scry <rustc> <args>`
    // when this binary is the rustc wrapper.
    if args.first().is_some_and(|arg| arg == "scry") {
        args.remove(0);
        return subcommand(args);
    }
    if args.is_empty() {
        eprintln!("{}", USAGE);
        return ExitCode::FAILURE;
    }
    let rustc = args.remove(0);
    ExitCode::from(wrapper::run(rustc, args))
}

const USAGE: &str = "\
cargo scry <cargo command> [args]

Runs the cargo command (build, run, test, check, ...) with --features scry, generating a Scry.Rust
registry for every workspace crate that has a `scry` feature and depends on `scry`.

Example: cargo scry run -p console-demo";

fn subcommand(args: Vec<OsString>) -> ExitCode {
    let Some((command, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    if command == "--help" || command == "-h" {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("error: cargo scry cannot find its own executable: {error}");
            return ExitCode::FAILURE;
        }
    };
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    let mut cmd = Command::new(&cargo);
    cmd.arg(command).arg("--features").arg("scry").args(rest);
    cmd.env("RUSTC_WORKSPACE_WRAPPER", &exe).env(wrapper::GENERATOR_ID, generator_id());
    if env::var_os("CARGO_TARGET_DIR").is_none()
        && !rest.iter().any(|arg| arg.to_string_lossy().starts_with("--target-dir"))
        && let Some(target) = target_directory(&cargo)
    {
        cmd.env("CARGO_TARGET_DIR", target.join("scry"));
    }

    match cmd.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("error: cargo scry could not run cargo: {error}");
            ExitCode::FAILURE
        }
    }
}

fn target_directory(cargo: &OsString) -> Option<PathBuf> {
    let output = Command::new(cargo).args(["metadata", "--format-version", "1", "--no-deps"]).output().ok()?;
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    metadata["target_directory"].as_str().map(PathBuf::from)
}
