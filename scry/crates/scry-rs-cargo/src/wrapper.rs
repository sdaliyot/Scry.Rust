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

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::rewrite::{Options, Paths, normalize, rewrite_crate};

/// Set by `cargo scry` to an id of the generator build. Recorded as an environment dependency of
/// each rewritten crate, so cargo rebuilds it when the generator changes.
pub const GENERATOR_ID: &str = "SCRY_GENERATOR_ID";

/// rustc options whose value is the next argument, so it is not taken for the source file.
const TAKES_VALUE: &[&str] = &[
    "--crate-name", "--edition", "--crate-type", "--emit", "--out-dir", "-o", "-L", "-l", "--extern", "--cfg",
    "--check-cfg", "-C", "--codegen", "--cap-lints", "--error-format", "--json", "--target", "--remap-path-prefix",
    "-A", "-W", "-D", "-F", "--force-warn", "--print", "--diagnostic-width", "--sysroot", "--color", "-Z", "--explain",
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
            if arg == option {
                self.args.get(index + 1).map(String::as_str)
            } else {
                arg.strip_prefix(joined.as_str())
            }
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
        self.source.is_some() && self.values("--cfg").any(|value| value == "feature=\"scry\"") && self.has_extern("scry")
    }
}

pub fn run(rustc: OsString, args: Vec<OsString>) -> u8 {
    let args: Vec<String> = args.into_iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
    let invocation = Invocation::parse(args);
    if !invocation.opted_in() {
        return exec(&rustc, &invocation.args);
    }
    match rewritten(&invocation) {
        Ok((args, dep_info, mappings)) => {
            let code = exec(&rustc, &args);
            if code == 0
                && let Some(dep_info) = dep_info
            {
                fix_dep_info(&dep_info, &mappings);
            }
            code
        }
        Err(error) => {
            // A rewrite failure must not break the build: compile the originals, which still
            // gives a working app, just with fewer roots.
            eprintln!("warning: scry: not generating a registry for this crate: {error}");
            exec(&rustc, &invocation.args)
        }
    }
}

type Rewritten = (Vec<String>, Option<PathBuf>, Vec<(String, String)>);

fn rewritten(invocation: &Invocation) -> Result<Rewritten, String> {
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
    let options = Options {
        dioxus: invocation.has_extern("dioxus"),
        // A test harness supplies its own `main`.
        executable: invocation.values("--crate-type").any(|kind| kind == "bin") && !invocation.args.iter().any(|arg| arg == "--test"),
    };
    let output = rewrite_crate(&root, &paths, &options)?;

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
    Ok((args, dep_info, mappings))
}

fn exec(rustc: &OsString, args: &[String]) -> u8 {
    match Command::new(rustc).args(args).status() {
        Ok(status) => status.code().unwrap_or(1).clamp(0, 255) as u8,
        Err(error) => {
            eprintln!("error: scry: could not run {}: {error}", rustc.to_string_lossy());
            1
        }
    }
}

/// Points the dependency file at the original sources, so cargo watches those, and records the
/// generator id as an environment dependency.
fn fix_dep_info(path: &Path, mappings: &[(String, String)]) {
    let Ok(mut text) = fs::read_to_string(path) else { return };
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
