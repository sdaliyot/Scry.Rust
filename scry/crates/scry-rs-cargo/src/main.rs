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
//!
//! `cargo scry clippy [args] [-- lint args]` lints the rewritten code. Clippy normally takes the
//! same wrapper slot, so `cargo scry` runs `cargo check` itself and the wrapper hands each
//! compilation on to `clippy-driver`, the way `cargo clippy` would.

mod config;
mod edits;
mod features;
mod functions;
mod generate;
mod generics;
mod hookpoints;
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
    // `cargo scry dx` runs this binary as `rustc`; see `dx_rustc`.
    if is_rustc_shim() {
        let Some(rustc) = env::var_os(REAL_RUSTC) else {
            eprintln!("error: scry: running as rustc, but {REAL_RUSTC} is not set");
            return ExitCode::FAILURE;
        };
        return ExitCode::from(wrapper::run(rustc, args));
    }
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

Runs the cargo command (build, run, test, check, clippy, ...) with --features scry, generating a
Scry.Rust registry for every workspace crate that has a `scry` feature and depends on `scry`.

Examples: cargo scry run -p console-demo
          cargo scry clippy -p console-demo -- -D warnings

cargo scry dx <dx command> [args]

Runs the Dioxus CLI the same way, for apps whose assets only dx bundles.

Example:  cargo scry dx build --platform desktop --features desktop";

/// The real rustc, for this binary running as `rustc` under `cargo scry dx`.
const REAL_RUSTC: &str = "SCRY_REAL_RUSTC";

fn is_rustc_shim() -> bool {
    env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|stem| stem.eq_ignore_ascii_case("rustc")))
        .unwrap_or(false)
}

/// Sets `cmd` up so the workspace crates dx compiles go through the wrapper.
///
/// dx takes the rustc wrapper slot itself, and its wrapper then runs `rustc` by name. So a copy of
/// this binary named `rustc` goes first on the `PATH`, and finds the real one through
/// [`REAL_RUSTC`]. cargo's own `RUSTC` is pointed at the real one too, so dependencies, build
/// scripts and cargo's probes never see the copy.
fn dx_rustc(cmd: &mut Command, exe: &std::path::Path) -> Result<(), String> {
    let real = match env::var_os("RUSTC") {
        Some(rustc) => PathBuf::from(rustc),
        None => {
            let output = Command::new("rustc")
                .args(["--print", "sysroot"])
                .output()
                .map_err(|error| format!("cannot run rustc: {error}"))?;
            let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !output.status.success() || sysroot.is_empty() {
                return Err("cannot find the rustc sysroot".into());
            }
            PathBuf::from(sysroot).join("bin").join(format!("rustc{}", env::consts::EXE_SUFFIX))
        }
    };
    // One copy per generator build, so a copy in use by another build is never overwritten.
    let dir = env::temp_dir().join("scry-rustc").join(generator_id());
    let shim = dir.join(format!("rustc{}", env::consts::EXE_SUFFIX));
    if !shim.is_file() {
        std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let partial = dir.join(format!("rustc.{}.partial", std::process::id()));
        std::fs::copy(exe, &partial).map_err(|error| format!("{}: {error}", partial.display()))?;
        if std::fs::rename(&partial, &shim).is_err() && !shim.is_file() {
            return Err(format!("cannot create {}", shim.display()));
        }
        let _ = std::fs::remove_file(&partial);
    }
    let path = env::var_os("PATH").unwrap_or_default();
    let path = env::join_paths(std::iter::once(dir).chain(env::split_paths(&path)))
        .map_err(|error| format!("PATH: {error}"))?;
    cmd.env("PATH", path).env(REAL_RUSTC, &real).env("RUSTC", &real);
    Ok(())
}

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

    let dx = command == "dx";
    let (command, rest) = match (dx, rest.split_first()) {
        (false, _) => (command, rest),
        (true, Some(split)) => split,
        (true, None) => {
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let mut cmd = Command::new(if dx { "dx".into() } else { cargo.clone() });
    if dx {
        if let Err(error) = dx_rustc(&mut cmd, &exe) {
            eprintln!("error: cargo scry dx: {error}");
            return ExitCode::FAILURE;
        }
        cmd.arg(command).arg("--features").arg("scry").args(rest);
    } else if command == "clippy" {
        // Lint arguments come after `--`; cargo-clippy passes them to clippy-driver this way.
        let split = rest.iter().position(|arg| arg == "--").unwrap_or(rest.len());
        let lint_args: String = rest[(split + 1).min(rest.len())..]
            .iter()
            .map(|arg| format!("{}__CLIPPY_HACKERY__", arg.to_string_lossy()))
            .collect();
        cmd.arg("check").arg("--features").arg("scry").args(&rest[..split]);
        cmd.env(wrapper::NEXT_WRAPPER, "clippy-driver").env("CLIPPY_ARGS", lint_args);
    } else {
        cmd.arg(command).arg("--features").arg("scry").args(rest);
    }
    // Lock support of scry-rs and serde support of the app's dependencies; see `features`.
    let own_args = &rest[..rest.iter().position(|arg| arg == "--").unwrap_or(rest.len())];
    let metadata = metadata(&cargo, own_args);
    if let Some(metadata) = &metadata {
        let cwd = env::current_dir().unwrap_or_default();
        match features::extra(metadata, own_args, &cwd) {
            Ok(extra) => {
                if env::var_os("SCRY_VERBOSE").is_some() && !extra.is_empty() {
                    eprintln!("cargo scry: adding features {}", extra.join(" "));
                }
                for feature in extra {
                    cmd.arg("--features").arg(feature);
                }
            }
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    cmd.env("RUSTC_WORKSPACE_WRAPPER", &exe).env(wrapper::GENERATOR_ID, generator_id());
    if env::var_os("CARGO_TARGET_DIR").is_none()
        && !own_args.iter().any(|arg| arg.to_string_lossy().starts_with("--target-dir"))
        && let Some(target) = metadata.as_ref().and_then(|metadata| metadata["target_directory"].as_str())
    {
        cmd.env("CARGO_TARGET_DIR", PathBuf::from(target).join("scry"));
    }
    match cmd.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("error: cargo scry could not run cargo: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `cargo metadata` for the workspace the command builds, honouring its `--manifest-path`.
fn metadata(cargo: &OsString, args: &[OsString]) -> Option<serde_json::Value> {
    let mut cmd = Command::new(cargo);
    cmd.args(["metadata", "--format-version", "1"]);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if text == "--manifest-path" {
            cmd.arg(arg).args(args.next());
        } else if text.starts_with("--manifest-path=") {
            cmd.arg(arg);
        }
    }
    let output = cmd.stderr(std::process::Stdio::null()).output().ok()?;
    serde_json::from_slice(&output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn dx_mode_puts_a_rustc_copy_first_on_the_path() {
        let source = env::temp_dir().join(format!("scry-dx-test-{}.bin", std::process::id()));
        std::fs::write(&source, b"stand-in for cargo-scry").unwrap();
        let mut cmd = Command::new("dx");
        dx_rustc(&mut cmd, &source).unwrap();
        let _ = std::fs::remove_file(&source);

        let env = |name: &str| {
            cmd.get_envs()
                .find(|(key, _)| *key == OsStr::new(name))
                .and_then(|(_, value)| value.map(OsStr::to_os_string))
                .unwrap_or_else(|| panic!("{name} not set"))
        };
        let first = env::split_paths(&env("PATH")).next().unwrap();
        let shim = first.join(format!("rustc{}", env::consts::EXE_SUFFIX));
        assert!(shim.is_file(), "{} missing", shim.display());
        let real = PathBuf::from(env(REAL_RUSTC));
        assert!(real.is_file(), "{} missing", real.display());
        assert_ne!(real, shim);
        // cargo itself, and so every dependency and build script, uses the real rustc.
        assert_eq!(env("RUSTC"), real.as_os_str());
        let _ = std::fs::remove_dir_all(&first);
    }
}
