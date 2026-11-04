//! Rewrites one crate: walks its module tree from the root file, applies [`generate`] to every
//! file, and writes the results into a mirror directory that rustc compiles instead.
//!
//! Each rewritten `mod name;` gets an explicit `#[path]` to its rewritten file, and each relative
//! `include_str!`-style path is made absolute, so nothing depends on where the mirror lives. Files
//! under the package keep their relative layout in the mirror (`src/main.rs`); anything else goes
//! under `_external/<drive>/...`.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use syn::visit::Visit;

use crate::edits::{Edits, Source};
use crate::generate::{
    Context, Generator, LaunchCollector, ModCollector, ModDecl, Report, TraitCollector, resolve_dyn_traits,
};
use crate::tokens::{self, Include};

pub struct Output {
    /// The rewritten crate root, to compile instead of the original.
    pub root: PathBuf,
    /// The drives of files outside the package, which need their own path remapping.
    pub external_drives: Vec<String>,
}

pub struct Paths<'a> {
    pub manifest_dir: &'a Path,
    pub mirror_root: &'a Path,
    pub out_dir: Option<&'a Path>,
}

impl Paths<'_> {
    pub fn mirror_of(&self, original: &Path) -> PathBuf {
        match original.strip_prefix(self.manifest_dir) {
            Ok(relative) => self.mirror_root.join(relative),
            Err(_) => {
                let mut mirror = self.mirror_root.join("_external");
                for component in original.components() {
                    match component {
                        Component::Prefix(prefix) => {
                            mirror.push(prefix.as_os_str().to_string_lossy().trim_end_matches(':'))
                        }
                        Component::Normal(part) => mirror.push(part),
                        _ => {}
                    }
                }
                mirror
            }
        }
    }
}

struct File {
    original: PathBuf,
    source: Source,
    syntax: syn::File,
    mods: Vec<(ModDecl, Option<PathBuf>)>,
    includes: Vec<Include>,
}

/// What kind of crate is being rewritten.
pub struct Options {
    /// It depends on Dioxus.
    pub dioxus: bool,
    /// It is an executable with a `main` to start the endpoint from.
    pub executable: bool,
}

pub fn rewrite_crate(root: &Path, paths: &Paths<'_>, options: &Options) -> Result<Output, String> {
    let files = load(root, paths)?;

    let mut report = Report::default();
    let mut traits = TraitCollector::default();
    for file in &files {
        traits.visit_file(&file.syntax);
    }
    let dyn_traits = resolve_dyn_traits(&traits.traits, &mut report);
    let mut launches = LaunchCollector::default();
    launches.visit_file(&files[0].syntax);
    let cx = Context {
        dyn_traits: &dyn_traits,
        hooks: options.dioxus,
        endpoint: options.executable.then_some(&launches.roots),
    };

    let _ = fs::remove_dir_all(paths.mirror_root);
    let mut external_drives = HashSet::new();
    for (index, file) in files.iter().enumerate() {
        report.lines.push(format!("{}", file.original.display()));
        let mut edits = Edits::default();
        Generator::new(&file.source, &mut edits, &cx, &mut report, index == 0).visit_file(&file.syntax);

        for (decl, child) in &file.mods {
            let Some(child) = child else { continue };
            let literal = format!("{:?}", paths.mirror_of(child).to_string_lossy());
            match decl.path_literal {
                Some((start, end)) => edits.replace(start, end, literal),
                None => edits.insert(decl.insert_at, format!("#[path = {literal}] ")),
            }
        }
        let dir = file.original.parent().unwrap_or(Path::new("."));
        for include in &file.includes {
            match include {
                Include::Relative { start, end, path } => {
                    edits.replace(*start, *end, format!("{:?}", normalize(&dir.join(path)).to_string_lossy()));
                }
                Include::BuildOutput { start, end, file: name } => {
                    if let Some(out_dir) = paths.out_dir {
                        let target = paths.mirror_of(&out_dir.join(name));
                        edits.replace(*start, *end, format!("include!({:?})", target.to_string_lossy()));
                    }
                }
            }
        }

        let mirror = paths.mirror_of(&file.original);
        if !file.original.starts_with(paths.manifest_dir)
            && let Some(Component::Prefix(prefix)) = file.original.components().next()
        {
            external_drives.insert(prefix.as_os_str().to_string_lossy().trim_end_matches(':').to_owned());
        }
        if let Some(parent) = mirror.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        fs::write(&mirror, edits.apply(&file.source.text)).map_err(|error| format!("{}: {error}", mirror.display()))?;
    }

    let report = format!(
        "scry generator report\n\
         Lines starting with + were generated, - were skipped. The rewritten sources are next to this file.\n\n{}\n",
        report.lines.join("\n")
    );
    let _ = fs::write(paths.mirror_root.join("scry-report.txt"), report);

    Ok(Output { root: paths.mirror_of(root), external_drives: external_drives.into_iter().collect() })
}

/// Reads and parses the module tree, plus files a build script generated that the crate includes.
fn load(root: &Path, paths: &Paths<'_>) -> Result<Vec<File>, String> {
    let mut files = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), true)]);
    let mut seen = HashSet::new();
    while let Some((path, mod_rs)) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let source = Source::new(text);
        let syntax = syn::parse_file(&source.text).map_err(|error| {
            let start = error.span().start();
            format!("{}:{}:{}: {error}", path.display(), start.line, start.column + 1)
        })?;

        let mut collector = ModCollector::new(&source);
        collector.visit_file(&syntax);
        let mods: Vec<(ModDecl, Option<PathBuf>)> = collector
            .decls
            .into_iter()
            .map(|decl| {
                let child = resolve_mod(&path, mod_rs, &decl);
                if let Some((child, child_mod_rs)) = &child {
                    queue.push_back((child.clone(), *child_mod_rs));
                }
                (decl, child.map(|(child, _)| child))
            })
            .collect();

        let includes = tokens::includes(&source);
        if let Some(out_dir) = paths.out_dir {
            for include in &includes {
                if let Include::BuildOutput { file, .. } = include {
                    queue.push_back((out_dir.join(file), false));
                }
            }
        }

        files.push(File { original: path, source, syntax, mods, includes });
    }
    Ok(files)
}

/// Where `mod name;` in `file` loads from, and whether that file owns a directory the way
/// `mod.rs`/`main.rs`/`lib.rs` do. `None` when no file exists, which is fine for a declaration a
/// `#[cfg]` turns off.
fn resolve_mod(file: &Path, mod_rs: bool, decl: &ModDecl) -> Option<(PathBuf, bool)> {
    let dir = file.parent()?;
    let mut base = if mod_rs { dir.to_path_buf() } else { dir.join(file.file_stem()?) };
    for inline in &decl.inline_path {
        base.push(inline);
    }
    if let Some(path) = &decl.path_attr {
        let relative_to = if decl.inline_path.is_empty() { dir } else { &base };
        let target = normalize(&relative_to.join(path));
        let owns_dir = target.file_name().is_some_and(|name| name == "mod.rs");
        return target.is_file().then_some((target, owns_dir));
    }
    let flat = base.join(format!("{}.rs", decl.name));
    if flat.is_file() {
        return Some((flat, false));
    }
    let nested = base.join(&decl.name).join("mod.rs");
    nested.is_file().then_some((nested, true))
}

/// Resolves `.` and `..` without touching the filesystem (`canonicalize` would turn a Windows
/// path into a `\\?\` one that rustc reports verbatim).
pub fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            other => result.push(other),
        }
    }
    result
}
