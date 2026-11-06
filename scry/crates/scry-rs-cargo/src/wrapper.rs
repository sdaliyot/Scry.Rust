//! The rustc wrapper half of `cargo scry`.
//!
//! cargo runs it as `cargo-scry <rustc> <args>` for each workspace crate. A crate is rewritten
//! only when it is built with `--cfg feature="scry"` and an `--extern scry=...`: it opted in and
//! can name the library the generated code refers to. Everything else - build scripts, crates
//! without the feature, `rustc -vV` probes - runs rustc unchanged.
//!
//! For a rewritten crate rustc compiles the mirror, with `--remap-path-prefix` mapping it back to
//! the original paths in diagnostics, panics and debug info, and the dependency file rustc writes
//! for cargo is rewritten the same way, so editing an original file triggers a rebuild.

use std::cell::OnceCell;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::rewrite::{Options, Paths, normalize, rewrite_crate};

/// Set by `cargo scry` to an id of the generator build. Recorded as an environment dependency of
/// each rewritten crate, so cargo rebuilds it when the generator changes.
pub const GENERATOR_ID: &str = "SCRY_GENERATOR_ID";

/// A wrapper to run rustc through after this one, as `<wrapper> <rustc> <args>`: `clippy-driver`
/// for `cargo scry clippy`.
pub const NEXT_WRAPPER: &str = "SCRY_NEXT_WRAPPER";

/// Set to show the output of compile attempts that were thrown away (see [`run`]), to see why
/// generated code did not compile.
pub const VERBOSE: &str = "SCRY_VERBOSE";

/// rustc options whose value is the next argument, so it is not taken for the source file.
const TAKES_VALUE: &[&str] = &[
    "--crate-name",
    "--edition",
    "--crate-type",
    "--emit",
    "--out-dir",
    "-o",
    "-L",
    "-l",
    "--extern",
    "--cfg",
    "--check-cfg",
    "-C",
    "--codegen",
    "--cap-lints",
    "--error-format",
    "--json",
    "--target",
    "--remap-path-prefix",
    "-A",
    "-W",
    "-D",
    "-F",
    "--force-warn",
    "--print",
    "--diagnostic-width",
    "--sysroot",
    "--color",
    "-Z",
    "--explain",
];

struct Invocation {
    args: Vec<String>,
    source: Option<usize>,
}

impl Invocation {
    fn parse(args: Vec<String>) -> Self {
        let mut source = None;
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            if TAKES_VALUE.contains(&arg.as_str()) {
                index += 2;
                continue;
            }
            if !arg.starts_with('-') && arg.ends_with(".rs") {
                source = Some(index);
            }
            index += 1;
        }
        Self { args, source }
    }

    /// Values of an option, written either `--opt value` or `--opt=value`.
    fn values<'a>(&'a self, option: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let joined = format!("{option}=");
        self.args.iter().enumerate().filter_map(move |(index, arg)| {
            if arg == option { self.args.get(index + 1).map(String::as_str) } else { arg.strip_prefix(joined.as_str()) }
        })
    }

    fn codegen(&self, key: &str) -> Option<&str> {
        let prefix = format!("{key}=");
        self.values("-C").chain(self.values("--codegen")).find_map(|value| value.strip_prefix(prefix.as_str()))
    }

    fn has_extern(&self, name: &str) -> bool {
        let prefix = format!("{name}=");
        self.values("--extern").any(|value| value == name || value.starts_with(&prefix))
    }

    fn opted_in(&self) -> bool {
        self.source.is_some()
            && self.values("--cfg").any(|value| value == "feature=\"scry\"")
            && self.has_extern("scry")
    }
}

/// Compiles one crate: rewritten if it opted in, as is otherwise.
///
/// Generated code must never be what breaks a build. So a rewritten crate that fails to compile is
/// compiled again without hook points, and if that fails too, from its original sources. Each
/// attempt's output is held back until it is the one kept, so a real error in the app's own code
/// is reported once, against the original files, and only a failure of the generated code warns.
pub fn run(rustc: OsString, args: Vec<OsString>) -> u8 {
    let args: Vec<String> = args.into_iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
    let invocation = Invocation::parse(args);
    if !invocation.opted_in() {
        return exec(&rustc, &invocation.args);
    }
    let crate_name = invocation.values("--crate-name").next().unwrap_or("crate").to_owned();
    let mir = OnceCell::new();
    let mut first_error = None;
    let mut fresh_generics = false;
    let mut attempts = std::collections::VecDeque::from([true, false]);
    while let Some(hook_points) = attempts.pop_front() {
        let attempt = match rewritten(&rustc, &invocation, hook_points, fresh_generics, &mir) {
            Ok(attempt) => attempt,
            Err(error) => {
                eprintln!("warning: scry: not generating a registry for `{crate_name}`: {error}");
                return exec(&rustc, &invocation.args);
            }
        };
        if hook_points && attempt.hook_points == 0 && !attempt.generics_reused {
            // Nothing to leave out on a retry.
            return finish(&rustc, &attempt, exec(&rustc, &attempt.args));
        }
        let (code, stdout, stderr) = exec_captured(&rustc, &attempt.args);
        if code == 0 {
            replay(&stdout, &stderr);
            if let Some(error) = &first_error {
                eprintln!(
                    "warning: scry: `{crate_name}` did not compile with hook points, so it was built without them ({error}). \
                     Please report this, with the function named in the error."
                );
            }
            return finish(&rustc, &attempt, 0);
        }
        if attempt.generics_reused {
            // The last build's generic instantiations may be out of date in a way the key can't
            // see. Find them again before blaming the generated code.
            if env::var_os(VERBOSE).is_some() {
                eprintln!("scry: attempt for `{crate_name}` with the last build's generic instantiations failed:");
                replay(&stdout, &stderr);
            }
            fresh_generics = true;
            attempts.push_front(hook_points);
            continue;
        }
        first_error.get_or_insert_with(|| first_error_message(&stderr));
        if env::var_os(VERBOSE).is_some() {
            eprintln!(
                "scry: attempt {} for `{crate_name}` failed:",
                if hook_points { "with hook points" } else { "without hook points" }
            );
            replay(&stdout, &stderr);
        }
    }
    let (code, stdout, stderr) = exec_captured(&rustc, &invocation.args);
    replay(&stdout, &stderr);
    if code == 0 {
        let error = first_error.unwrap_or_default();
        eprintln!(
            "warning: scry: the code generated for `{crate_name}` did not compile, so it was built without scry ({error}). \
             Please report this."
        );
    }
    code
}

fn finish(_rustc: &OsString, attempt: &Rewritten, code: u8) -> u8 {
    if code == 0
        && let Some(dep_info) = &attempt.dep_info
    {
        fix_dep_info(dep_info, &attempt.mappings, &attempt.manifests);
    }
    code
}

/// The arguments to compile a rewritten crate with, and what to fix up afterwards.
struct Rewritten {
    args: Vec<String>,
    dep_info: Option<PathBuf>,
    mappings: Vec<(String, String)>,
    /// Manifests whose `[package.metadata.scry]` shaped the rewrite: cargo does not rebuild for a
    /// metadata change on its own, so they are added to the dependency file.
    manifests: Vec<PathBuf>,
    hook_points: usize,
    generics_reused: bool,
}

fn rewritten(
    rustc: &OsString,
    invocation: &Invocation,
    hook_points: bool,
    fresh_generics: bool,
    mir: &OnceCell<Result<String, String>>,
) -> Result<Rewritten, String> {
    let cwd = env::current_dir().map_err(|error| error.to_string())?;
    let source_index = invocation.source.ok_or("no source file")?;
    let root = normalize(&cwd.join(&invocation.args[source_index]));
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(|dir| normalize(&cwd.join(dir)))
        .or_else(|| root.parent().map(Path::to_path_buf))
        .ok_or("no package directory")?;
    let crate_name = invocation.values("--crate-name").next().unwrap_or("crate");
    let extra = invocation.codegen("extra-filename").unwrap_or("");
    let out_dir = invocation.values("--out-dir").next().map(|dir| normalize(&cwd.join(dir))).ok_or("no --out-dir")?;
    let mirror_root = out_dir.parent().unwrap_or(&out_dir).join("scry-src").join(format!("{crate_name}{extra}"));
    let build_out_dir = env::var_os("OUT_DIR").map(PathBuf::from);

    let paths = Paths { manifest_dir: &manifest_dir, mirror_root: &mirror_root, out_dir: build_out_dir.as_deref() };
    let config = Config::load(&manifest_dir)?;
    let manifests = config.manifests.clone();
    let options = Options {
        crate_name: crate_name.to_owned(),
        config,
        hook_points,
        fresh_generics,
        generics_seed: format!(
            "{}\n{}\n{}",
            rustc.to_string_lossy(),
            env::var(GENERATOR_ID).unwrap_or_default(),
            invocation.args.join("\n")
        ),
        dioxus: invocation.has_extern("dioxus"),
        // A test harness supplies its own `main`.
        executable: invocation.values("--crate-type").any(|kind| kind == "bin")
            && !invocation.args.iter().any(|arg| arg == "--test"),
    };
    let mir_path = mirror_root.with_extension("mir");
    let mir = || mir.get_or_init(|| mir_dump(rustc, invocation, &mir_path)).clone();
    let output = rewrite_crate(&root, &paths, &options, &mir)?;

    // Later mappings win in rustc, so the more specific `_external` ones come last.
    let mut mappings = vec![(mirror_root.display().to_string(), manifest_dir.display().to_string())];
    for drive in &output.external_drives {
        mappings.push((mirror_root.join("_external").join(drive).display().to_string(), format!("{drive}:")));
    }

    let mut args = invocation.args.clone();
    args[source_index] = output.root.display().to_string();
    for (from, to) in &mappings {
        args.push(format!("--remap-path-prefix={from}={to}"));
    }

    let dep_info = invocation.values("--emit").find_map(|emit| {
        emit.split(',').find_map(|kind| match kind.strip_prefix("dep-info") {
            Some("") => Some(out_dir.join(format!("{crate_name}{extra}.d"))),
            Some(path) => path.strip_prefix('=').map(PathBuf::from),
            None => None,
        })
    });
    Ok(Rewritten {
        args,
        dep_info,
        mappings,
        manifests,
        hook_points: output.hook_points,
        generics_reused: output.generics_reused,
    })
}

/// Compiles the original sources once more, to MIR only, for the generic instantiations they use.
/// The same arguments as the real build, minus its outputs, without optimizations (MIR inlining
/// would hide calls), and with lints and diagnostics silenced. It keeps an incremental cache of its
/// own next to the dump, so after a small edit rustc redoes only what the edit touched.
fn mir_dump(rustc: &OsString, invocation: &Invocation, path: &Path) -> Result<String, String> {
    let mut args = Vec::new();
    let mut index = 0;
    while index < invocation.args.len() {
        let arg = &invocation.args[index];
        let value = invocation.args.get(index + 1).map(String::as_str).unwrap_or("");
        let dropped_pair = matches!(arg.as_str(), "--emit" | "-o" | "--out-dir" | "--error-format" | "--json")
            || matches!(arg.as_str(), "-C" | "--codegen")
                && (value.starts_with("incremental=") || value.starts_with("opt-level="));
        if dropped_pair {
            index += 2;
            continue;
        }
        let dropped =
            ["--emit=", "-o", "--out-dir=", "--error-format=", "--json="].iter().any(|flag| arg.starts_with(flag))
                || arg.starts_with("-Cincremental=")
                || arg.starts_with("-Copt-level=");
        if !dropped {
            args.push(arg.clone());
        }
        index += 1;
    }
    args.push(format!("--emit=mir={}", path.display()));
    args.push(format!("-Cincremental={}", path.with_extension("incremental").display()));
    args.extend(["-C".into(), "opt-level=0".into(), "--cap-lints=allow".into()]);
    let output = Command::new(rustc).args(&args).output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("the crate does not compile as written".into());
    }
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let _ = fs::remove_file(path);
    Ok(text)
}

fn command(rustc: &OsString, args: &[String]) -> Command {
    let mut command = match env::var_os(NEXT_WRAPPER) {
        Some(next) => {
            let mut command = Command::new(next);
            command.arg(rustc);
            command
        }
        None => Command::new(rustc),
    };
    command.args(args);
    command
}

/// Runs rustc with its output held back, for an attempt that may be thrown away.
fn exec_captured(rustc: &OsString, args: &[String]) -> (u8, Vec<u8>, Vec<u8>) {
    match command(rustc, args).output() {
        Ok(output) => (output.status.code().unwrap_or(1).clamp(0, 255) as u8, output.stdout, output.stderr),
        Err(error) => {
            (1, Vec::new(), format!("error: scry: could not run {}: {error}\n", rustc.to_string_lossy()).into_bytes())
        }
    }
}

fn replay(stdout: &[u8], stderr: &[u8]) {
    let _ = std::io::stdout().write_all(stdout);
    let _ = std::io::stderr().write_all(stderr);
}

/// The first error rustc reported, from its JSON diagnostics (cargo asks for those) or its text.
fn first_error_message(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines() {
        if let Ok(diagnostic) = serde_json::from_str::<serde_json::Value>(line)
            && diagnostic["level"] == "error"
        {
            let message = diagnostic["message"].as_str().unwrap_or("an error");
            let at = &diagnostic["spans"][0];
            return match at["file_name"].as_str() {
                Some(file) => format!("{message} at {file}:{}", at["line_start"]),
                None => message.to_owned(),
            };
        }
        if let Some(message) = line.strip_prefix("error") {
            return format!("error{message}");
        }
    }
    "an error".into()
}

fn exec(rustc: &OsString, args: &[String]) -> u8 {
    match command(rustc, args).status() {
        Ok(status) => status.code().unwrap_or(1).clamp(0, 255) as u8,
        Err(error) => {
            eprintln!("error: scry: could not run {}: {error}", rustc.to_string_lossy());
            1
        }
    }
}

/// Points the dependency file at the original sources, so cargo watches those, and records the
/// generator id as an environment dependency.
fn fix_dep_info(path: &Path, mappings: &[(String, String)], manifests: &[PathBuf]) {
    let Ok(mut text) = fs::read_to_string(path) else { return };
    // The first rule lists what the output depends on; the manifests join it.
    if let Some(end) = text.find('\n') {
        let extra: String = manifests
            .iter()
            .map(|manifest| format!(" {}", manifest.display().to_string().replace(' ', "\\ ")))
            .collect();
        text.insert_str(end, &extra);
    }
    for (from, to) in mappings.iter().rev() {
        // Make-style dependency files escape spaces in paths.
        for (from, to) in [(from.clone(), to.clone()), (from.replace(' ', "\\ "), to.replace(' ', "\\ "))] {
            text = text.replace(&format!("{from}\\"), &format!("{to}\\"));
            text = text.replace(&format!("{}/", from.replace('\\', "/")), &format!("{}/", to.replace('\\', "/")));
        }
    }
    if let Ok(id) = env::var(GENERATOR_ID) {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("# env-dep:{GENERATOR_ID}={id}\n"));
    }
    let _ = fs::write(path, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(args: &str) -> Invocation {
        Invocation::parse(args.split(' ').map(str::to_owned).collect())
    }

    #[test]
    fn opts_in_only_with_feature_and_extern() {
        let base = "--crate-name app --edition=2024 app/src/main.rs --crate-type bin";
        assert!(invocation(&format!("{base} --cfg feature=\"scry\" --extern scry=C:/t/libscry.rlib")).opted_in());
        assert!(!invocation(&format!("{base} --cfg feature=\"scry\"")).opted_in());
        assert!(!invocation(&format!("{base} --extern scry=x.rlib")).opted_in());
        assert!(!invocation("-vV").opted_in());
    }

    #[test]
    fn finds_the_source_and_codegen_options() {
        let parsed = invocation("--crate-name app src/main.rs -C extra-filename=-abc --out-dir target/deps");
        assert_eq!(parsed.source, Some(2));
        assert_eq!(parsed.codegen("extra-filename"), Some("-abc"));
        assert_eq!(parsed.values("--out-dir").next(), Some("target/deps"));
    }
}
