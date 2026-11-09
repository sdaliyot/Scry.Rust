//! Rewrites one crate: walks its module tree from the root file, applies [`generate`] to every
//! file, and writes the results into a mirror directory that rustc compiles instead.
//!
//! Each rewritten `mod name;` gets an explicit `#[path]` to its rewritten file, and each relative
//! `include_str!`-style path is made absolute, so nothing depends on where the mirror lives. Files
//! under the package keep their relative layout in the mirror (`src/main.rs`); anything else goes
//! under `_external/<drive>/...`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use quote::ToTokens;
use syn::visit::Visit;

use crate::cfg::Cfg;
use crate::config::Config;
use crate::edits::{Edits, Source};
use crate::generate::{
    Context, Generator, LaunchCollector, ModCollector, ModDecl, Report, TraitCollector, resolve_dyn_traits,
};
use crate::generics::{Index, scan};
use crate::tokens::{self, Include};

pub struct Output {
    /// The rewritten crate root, to compile instead of the original.
    pub root: PathBuf,
    /// The drives of files outside the package, which need their own path remapping.
    pub external_drives: Vec<String>,
    /// How many hook points were compiled in.
    pub hook_points: usize,
    /// The generic instantiations were the last build's, reused without a MIR pass.
    pub generics_reused: bool,
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
    /// The module path from the crate root (`["inventory"]`).
    module: Vec<String>,
    source: Source,
    syntax: syn::File,
    mods: Vec<(ModDecl, Option<PathBuf>)>,
    includes: Vec<Include>,
    /// A `#[cfg]` on its `mod` declaration (or an outer one's) leaves it out of this build.
    off: bool,
}

/// What kind of crate is being rewritten.
pub struct Options {
    /// It depends on Dioxus.
    pub dioxus: bool,
    /// It is an executable with a `main` to start the endpoint from.
    pub executable: bool,
    /// The features the build has on, to tell which code it compiles.
    pub cfg: Cfg,
    pub crate_name: String,
    /// The crate's `[package.metadata.scry]` settings.
    pub config: Config,
    /// Compile hook points in, as far as `config` allows. Off for the retry after they failed to
    /// compile.
    pub hook_points: bool,
    /// Find the generic instantiations with a MIR pass even when the last build's still apply.
    pub fresh_generics: bool,
    /// What identifies the compilation besides its sources (rustc, its arguments, the generator),
    /// for the key of the saved generic instantiations.
    pub generics_seed: String,
}

/// Rewrites the crate at `root`. `mir` compiles the original sources to a MIR dump, run only when
/// the crate has generic functions or types whose instantiations matter.
pub fn rewrite_crate(
    root: &Path,
    paths: &Paths<'_>,
    options: &Options,
    mir: &dyn Fn() -> Result<String, String>,
) -> Result<Output, String> {
    let files = load(root, paths, &options.cfg)?;

    let mut report = Report::default();
    let mut traits = TraitCollector::default();
    for file in &files {
        traits.visit_file(&file.syntax);
    }
    let dyn_traits = resolve_dyn_traits(&traits.traits, &mut report);
    let mut launches = LaunchCollector::new(&options.cfg);
    for file in files.iter().filter(|file| !file.off) {
        launches.visit_file(&file.syntax);
    }

    let mut index = Index::default();
    for file in &files {
        index.add_file(&file.module, &file.syntax);
    }
    let mut generics_reused = false;
    let uses = if index.generic_names.is_empty() {
        HashMap::new()
    } else if !options.config.generics {
        report.lines.push(
            "generic instantiations: off, by [package.metadata.scry.generics]; generic functions and types are skipped"
                .into(),
        );
        HashMap::new()
    } else {
        let saved = paths.mirror_root.with_extension("generics");
        let key = generics_key(&options.generics_seed, &files, &index.generic_names);
        let reused = if options.fresh_generics { None } else { read_saved(&saved, &key) };
        let calls = match reused {
            Some(calls) => {
                generics_reused = true;
                report.lines.push(
                    "generic instantiations: the last build's, since no generic code or code using it changed".into(),
                );
                Ok(calls)
            }
            None => mir().map(|text| {
                let calls = calls_of(&text, &index.generic_names);
                if let Some(parent) = saved.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&saved, format!("{key}\n{calls}"));
                calls
            }),
        };
        match calls {
            Ok(calls) => scan(&calls, &index.generic_names),
            Err(error) => {
                let _ = fs::remove_file(&saved);
                report.lines.push(format!("generic instantiations: not found, {error}"));
                HashMap::new()
            }
        }
    };
    let cx = Context {
        dyn_traits: &dyn_traits,
        hooks: options.dioxus,
        endpoint: options.executable.then_some(&launches.roots),
        index: &index,
        uses: &uses,
        generics: options.config.generics,
        crate_name: &options.crate_name,
        hook_points: (options.hook_points && options.config.hooks).then_some(&options.config),
        transparent: &options.config.transparent,
    };

    let _ = fs::remove_dir_all(paths.mirror_root);
    let mut external_drives = HashSet::new();
    let mut hook_points = 0;
    if !options.config.hooks {
        report.lines.push("hook points: off, by [package.metadata.scry.hooks]".into());
    } else if !options.hook_points {
        report.lines.push("hook points: off, because the crate did not compile with them".into());
    }
    for (index, file) in files.iter().enumerate() {
        report.lines.push(format!("{}", file.original.display()));
        let mut edits = Edits::default();
        let mut generator = Generator::new(&file.source, &mut edits, &cx, &mut report, index == 0, file.module.clone());
        generator.visit_file(&file.syntax);
        let points = generator.hook_points;
        if points > 0 {
            report.lines.push(format!("  + hook points: {points}"));
        }
        hook_points += points;

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

    Ok(Output {
        root: paths.mirror_of(root),
        external_drives: external_drives.into_iter().collect(),
        hook_points,
        generics_reused,
    })
}

/// A key for the generic instantiations a crate uses: what MIR shows of them can only change when
/// the compilation itself does (`seed`), or an item that defines or mentions a generic function or
/// type changes, or calls a method of a generic type. Every other item can change freely, and
/// so can comments and formatting.
///
/// An item that changes the inferred type at an unchanged call site (a field's type, a function's
/// return type) is missed when it names no generic: the old instantiation is kept, and the new one
/// is not found until the next MIR pass.
fn generics_key(seed: &str, files: &[File], names: &HashSet<String>) -> String {
    use std::hash::{Hash, Hasher};

    let mut watched = names.clone();
    for file in files {
        generic_type_methods(&file.syntax.items, names, &mut watched);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    let mut sorted: Vec<&String> = watched.iter().collect();
    sorted.sort();
    sorted.hash(&mut hasher);
    for file in files {
        file.module.hash(&mut hasher);
        hash_items(&file.syntax.items, &watched, &mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Adds the names of methods defined for generic types (`get` of `impl<K, V> Cache<K, V>`): a call
/// to one need not name the type.
fn generic_type_methods(items: &[syn::Item], names: &HashSet<String>, methods: &mut HashSet<String>) {
    for item in items {
        match item {
            syn::Item::Impl(item) if mentions(item.self_ty.to_token_stream(), names) => {
                for impl_item in &item.items {
                    if let syn::ImplItem::Fn(method) = impl_item {
                        methods.insert(method.sig.ident.to_string());
                    }
                }
            }
            syn::Item::Mod(item) => {
                if let Some((_, items)) = &item.content {
                    generic_type_methods(items, names, methods);
                }
            }
            _ => {}
        }
    }
}

fn hash_items(items: &[syn::Item], watched: &HashSet<String>, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;

    for item in items {
        match item {
            syn::Item::Mod(item) if item.content.is_some() => {
                item.ident.to_string().hash(hasher);
                if let Some((_, items)) = &item.content {
                    hash_items(items, watched, hasher);
                }
            }
            syn::Item::Impl(item) => {
                let generics = &item.generics;
                let self_ty = &item.self_ty;
                let trait_ = item.trait_.as_ref().map(|(_, path, _)| path);
                let header = quote::quote!(impl #generics #trait_ for #self_ty);
                if mentions(header.clone(), watched) {
                    item.to_token_stream().to_string().hash(hasher);
                    continue;
                }
                for impl_item in &item.items {
                    let tokens = impl_item.to_token_stream();
                    if mentions(tokens.clone(), watched) {
                        header.to_string().hash(hasher);
                        tokens.to_string().hash(hasher);
                    }
                }
            }
            item => {
                let tokens = item.to_token_stream();
                if mentions(tokens.clone(), watched) {
                    tokens.to_string().hash(hasher);
                }
            }
        }
    }
}

/// Whether `tokens` hold one of `names` as an identifier, at any depth.
fn mentions(tokens: proc_macro2::TokenStream, names: &HashSet<String>) -> bool {
    tokens.into_iter().any(|tree| match tree {
        proc_macro2::TokenTree::Ident(ident) => names.contains(&ident.to_string()),
        proc_macro2::TokenTree::Group(group) => mentions(group.stream(), names),
        _ => false,
    })
}

/// The lines of a MIR dump that can hold a use of one of `names`: all that [`scan`] needs, and much
/// smaller to keep.
fn calls_of(mir: &str, names: &HashSet<String>) -> String {
    let mut calls = String::new();
    for line in mir.lines().filter(|line| names.iter().any(|name| line.contains(name.as_str()))) {
        calls.push_str(line);
        calls.push('\n');
    }
    calls
}

/// The calls saved by the last build, if they were saved under `key`.
fn read_saved(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let (saved_key, calls) = text.split_once('\n')?;
    (saved_key == key).then(|| calls.to_owned())
}

/// Reads and parses the module tree, plus files a build script generated that the crate includes.
fn load(root: &Path, paths: &Paths<'_>, cfg: &Cfg) -> Result<Vec<File>, String> {
    let mut files = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), true, Vec::new(), false)]);
    let mut seen = HashSet::new();
    while let Some((path, mod_rs, module, off)) = queue.pop_front() {
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
                    let mut child_module = module.clone();
                    child_module.extend(decl.inline_path.iter().cloned());
                    child_module.push(decl.name.clone());
                    queue.push_back((child.clone(), *child_mod_rs, child_module, off || cfg.off(&decl.cfgs)));
                }
                (decl, child.map(|(child, _)| child))
            })
            .collect();

        let includes = tokens::includes(&source);
        if let Some(out_dir) = paths.out_dir {
            for include in &includes {
                if let Include::BuildOutput { file, .. } = include {
                    // An included file's items land in the including module.
                    queue.push_back((out_dir.join(file), false, module.clone(), off));
                }
            }
        }

        files.push(File { original: path, module, source, syntax, mods, includes, off });
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const MAIN: &str = "mod util;\n\
        fn main() { let mut v = vec![5i64]; util::clamp_all(&mut v, 0, 9); report(); }\n\
        fn report() { println!(\"one\"); }\n";
    const UTIL: &str = "pub fn clamp_all<T: PartialOrd + Copy>(values: &mut [T], lo: T, hi: T) {\n\
        for v in values { if *v < lo { *v = lo } else if *v > hi { *v = hi } }\n}\n\
        pub struct Cache<V> { pub value: Option<V> }\n\
        impl<V: Clone> Cache<V> { pub fn peek(&self) -> Option<V> { self.value.clone() } }\n";
    const MIR: &str = "fn main() -> () {\n\
        _1 = clamp_all::<i64>(move _2, const 0_i64, const 9_i64) -> [return: bb1];\n\
        _3 = report() -> [return: bb2];\n}\n";

    struct Crate {
        dir: PathBuf,
        mir_runs: Cell<usize>,
    }

    impl Crate {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("scry-rewrite-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(dir.join("src/main.rs"), MAIN).unwrap();
            fs::write(dir.join("src/util.rs"), UTIL).unwrap();
            Self { dir, mir_runs: Cell::new(0) }
        }

        fn edit(&self, file: &str, from: &str, to: &str) {
            let path = self.dir.join("src").join(file);
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.contains(from), "{from}");
            fs::write(&path, text.replace(from, to)).unwrap();
        }

        /// Rewrites the crate, returning whether the generic instantiations were reused and the report.
        fn rewrite(&self, config: Config, fresh_generics: bool) -> (bool, String) {
            let mirror = self.dir.join("target/scry-src/demo");
            let paths = Paths { manifest_dir: &self.dir, mirror_root: &mirror, out_dir: None };
            let options = Options {
                dioxus: false,
                executable: true,
                cfg: crate::cfg::Cfg::default(),
                crate_name: "demo".into(),
                config,
                hook_points: true,
                fresh_generics,
                generics_seed: "rustc --edition 2024".into(),
            };
            let mir = || {
                self.mir_runs.set(self.mir_runs.get() + 1);
                Ok(MIR.to_owned())
            };
            let output = rewrite_crate(&self.dir.join("src/main.rs"), &paths, &options, &mir).unwrap();
            (output.generics_reused, fs::read_to_string(mirror.join("scry-report.txt")).unwrap())
        }
    }

    impl Drop for Crate {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn generic_instantiations_are_found_again_only_when_generic_code_or_its_users_change() {
        let krate = Crate::new("generics");
        let (reused, report) = krate.rewrite(Config::default(), false);
        assert!(!reused);
        assert_eq!(krate.mir_runs.get(), 1);
        assert!(report.contains("clamp_all::<i64>"), "{report}");

        let (reused, report) = krate.rewrite(Config::default(), false);
        assert!(reused, "nothing changed");
        assert_eq!(krate.mir_runs.get(), 1);
        assert!(report.contains("clamp_all::<i64>"), "the saved instantiations are used: {report}");

        krate.edit("main.rs", "println!(\"one\")", "println!(\"two\") /* reworded */");
        krate.edit("util.rs", "pub value", "// The cached value.\n    pub value");
        assert!(krate.rewrite(Config::default(), false).0, "code without generics, and comments, changed");

        krate.edit("main.rs", "clamp_all(&mut v, 0, 9)", "clamp_all(&mut v, 1, 9)");
        assert!(!krate.rewrite(Config::default(), false).0, "a call of a generic function changed");
        assert_eq!(krate.mir_runs.get(), 2);

        krate.edit("main.rs", "report(); }", "report(); let _ = cache.peek(); }");
        assert!(!krate.rewrite(Config::default(), false).0, "a method of a generic type is called");
        krate.edit("util.rs", "*v = hi", "*v = hi;");
        assert!(!krate.rewrite(Config::default(), false).0, "a generic function changed");
        assert_eq!(krate.mir_runs.get(), 4);

        assert!(!krate.rewrite(Config::default(), true).0, "a fresh search was asked for");
        assert_eq!(krate.mir_runs.get(), 5);
    }

    #[test]
    fn generic_instantiations_can_be_turned_off() {
        let krate = Crate::new("no-generics");
        let mut config = Config::default();
        config.generics = false;
        let (reused, report) = krate.rewrite(config, false);
        assert!(!reused);
        assert_eq!(krate.mir_runs.get(), 0, "no MIR pass");
        assert!(report.contains("generic instantiations: off"), "{report}");
        assert!(!report.contains("clamp_all::<i64>"), "{report}");
        assert!(report.contains("fn clamp_all: skipped, generic, and generics are off"), "{report}");
        assert!(report.contains("impl Cache<V>: skipped, generic, and generics are off"), "{report}");
    }
}
