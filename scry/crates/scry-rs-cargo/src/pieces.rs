//! Generated code that can be left out one function at a time, when it doesn't compile.
//!
//! An attribute macro may rewrite the function it sits on in any way: keep it as written
//! (`tracing::instrument`), move its body into a handler of its own (a web framework's route
//! macros), or turn the function into a type. Which one, only compiling tells. So every function
//! gets its call shim and hook point whatever its attributes, each as a [`Piece`], and the
//! generated code is tagged with where each piece lands in the rewritten files. When the crate
//! doesn't compile, [`blame`] matches rustc's errors to the pieces they point at, through the
//! macro expansions they came from, and the crate is compiled again without those pieces. What was
//! left out is remembered ([`Dropped`]), so the next build doesn't try it again.

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// What was generated for a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    /// The call shim: calls the function by name, so it needs the function to stay one.
    Shim,
    /// The hook point: code at the start and end of the body, so it needs the body to stay one.
    HookPoint,
}

/// One function's call shim or hook point. `function` is its path in the crate: the module path
/// and the label the report uses (`api::auth::login`, `Inventory::sell_one`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Piece {
    pub function: String,
    pub part: Part,
}

impl Piece {
    pub fn new(module: &[String], label: &str, part: Part) -> Self {
        let mut function = module.join("::");
        if !function.is_empty() {
            function.push_str("::");
        }
        function.push_str(label);
        Self { function, part }
    }

    fn line(&self) -> String {
        let part = match self.part {
            Part::Shim => "shim",
            Part::HookPoint => "hook",
        };
        format!("{part} {}", self.function)
    }

    fn parse(line: &str) -> Option<Self> {
        let (part, function) = line.split_once(' ')?;
        let part = match part {
            "shim" => Part::Shim,
            "hook" => Part::HookPoint,
            _ => return None,
        };
        Some(Self { function: function.to_owned(), part })
    }
}

/// Where a piece landed in a rewritten file: a byte range of the file rustc compiles.
#[derive(Clone, Debug)]
pub struct Located {
    /// The original file, as rustc's diagnostics name it once the mirror is remapped back.
    pub original: PathBuf,
    /// The rewritten copy rustc compiles, in case a diagnostic names that instead.
    pub mirror: PathBuf,
    pub range: Range<usize>,
    pub piece: Piece,
    /// Original code marked as the piece's, rather than code generated for it: the function a
    /// hook point sits in, where an attribute macro may report trouble with it.
    pub marked: bool,
}

/// The pieces left out of a crate, and saved for its next build.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dropped {
    pub pieces: BTreeSet<Piece>,
}

impl Dropped {
    pub fn contains(&self, piece: &Piece) -> bool {
        self.pieces.contains(piece)
    }

    /// The last build's, if it was made by the same generator.
    pub fn load(path: &Path, generator: &str) -> Self {
        let Ok(text) = fs::read_to_string(path) else { return Self::default() };
        let mut lines = text.lines();
        if lines.next() != Some(generator) {
            return Self::default();
        }
        Self { pieces: lines.filter_map(Piece::parse).collect() }
    }

    pub fn save(&self, path: &Path, generator: &str) {
        if self.pieces.is_empty() {
            let _ = fs::remove_file(path);
            return;
        }
        let mut text = format!("{generator}\n");
        for piece in &self.pieces {
            text.push_str(&piece.line());
            text.push('\n');
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, text);
    }
}

/// What rustc's errors point at, among the pieces.
#[derive(Debug, Default, PartialEq)]
pub struct Blame {
    pub pieces: BTreeSet<Piece>,
    /// Errors that point at no piece: the app's own code, or generated code that isn't a piece.
    pub unexplained: usize,
}

/// Matches the errors in rustc's JSON diagnostics (`stderr`, one object per line) to the pieces
/// they point at: through any of their spans, or the macro expansions those came from. An error is
/// blamed on the pieces found the surest way, the first of: generated code at its primary spans,
/// marked code there, generated code at its other spans and its notes', marked code there. So an
/// error in a call shim that also points at the function it calls ("defined here") is the shim's,
/// not the hook point's whose function that is.
pub fn blame(stderr: &str, pieces: &[Located]) -> Blame {
    let mut blame = Blame::default();
    for line in stderr.lines() {
        let Ok(diagnostic) = serde_json::from_str::<Value>(line) else { continue };
        if diagnostic["level"] != "error" || diagnostic["spans"].as_array().is_none_or(Vec::is_empty) {
            continue;
        }
        let (mut primary, mut secondary) = (Vec::new(), Vec::new());
        for span in diagnostic["spans"].as_array().into_iter().flatten() {
            let list = if span["is_primary"] == true { &mut primary } else { &mut secondary };
            with_expansions(span, list);
        }
        for child in diagnostic["children"].as_array().into_iter().flatten() {
            collect_spans(child, &mut secondary);
        }
        let matching = |spans: &[&Value], marked: bool| -> BTreeSet<Piece> {
            spans
                .iter()
                .flat_map(|span| {
                    pieces.iter().filter(move |located| located.marked == marked && contains(located, span))
                })
                .map(|located| located.piece.clone())
                .collect()
        };
        let found = [(&primary, false), (&primary, true), (&secondary, false), (&secondary, true)]
            .into_iter()
            .map(|(spans, marked)| matching(spans, marked))
            .find(|found| !found.is_empty())
            .unwrap_or_default();
        if found.is_empty() {
            blame.unexplained += 1;
        }
        blame.pieces.extend(found);
    }
    blame
}

/// A diagnostic's spans, its children's, and every macro expansion's call site and definition.
fn collect_spans<'a>(diagnostic: &'a Value, spans: &mut Vec<&'a Value>) {
    for span in diagnostic["spans"].as_array().into_iter().flatten() {
        with_expansions(span, spans);
    }
    for child in diagnostic["children"].as_array().into_iter().flatten() {
        collect_spans(child, spans);
    }
}

/// A span, and the call site and definition of each macro expansion it came from.
fn with_expansions<'a>(span: &'a Value, spans: &mut Vec<&'a Value>) {
    let mut span = Some(span);
    while let Some(current) = span {
        spans.push(current);
        if let Some(def_site) = current["expansion"].get("def_site_span").filter(|span| span.is_object()) {
            spans.push(def_site);
        }
        span = current["expansion"].get("span").filter(|span| span.is_object());
    }
}

fn contains(located: &Located, span: &Value) -> bool {
    let Some(file) = span["file_name"].as_str() else { return false };
    let (Some(start), Some(end)) = (span["byte_start"].as_u64(), span["byte_end"].as_u64()) else { return false };
    // An empty span still marks a place.
    let (start, end) = (start as usize, (end as usize).max(start as usize + 1));
    (same_file(file, &located.original) || same_file(file, &located.mirror))
        && start < located.range.end
        && end > located.range.start
}

/// Whether a diagnostic's file name is `path`: rustc may print it with either kind of slash.
fn same_file(name: &str, path: &Path) -> bool {
    let normal = |text: &str| {
        let text = text.replace('\\', "/");
        if cfg!(windows) { text.to_lowercase() } else { text }
    };
    normal(name) == normal(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn located(range: Range<usize>, function: &str, part: Part) -> Located {
        Located {
            original: PathBuf::from("C:/app/src/api.rs"),
            mirror: PathBuf::from("C:/app/target/scry-src/app/src/api.rs"),
            range,
            piece: Piece { function: function.into(), part },
            marked: false,
        }
    }

    fn span(file: &str, start: usize, end: usize) -> Value {
        json!({ "file_name": file, "byte_start": start, "byte_end": end, "expansion": null, "is_primary": true })
    }

    fn parts(stderr: &Value, pieces: &[Located]) -> Vec<Part> {
        blame(&stderr.to_string(), pieces).pieces.iter().map(|piece| piece.part).collect()
    }

    #[test]
    fn an_error_is_blamed_on_what_it_points_at_first() {
        let mut function = located(0..90, "api::index", Part::HookPoint);
        function.marked = true;
        let pieces = [function, located(100..200, "api::index", Part::Shim)];
        // "expected function, found struct `index`" at the shim, and "`index` defined here" in
        // the function: the shim's alone.
        let mut defined_here = span("C:/app/src/api.rs", 10, 15);
        defined_here["is_primary"] = json!(false);
        let error =
            json!({ "level": "error", "spans": [span("C:/app/src/api.rs", 120, 125), defined_here], "children": [] });
        assert_eq!(parts(&error, &pieces), [Part::Shim]);
        // An error at the function's attribute only: the hook point's, through the mark.
        let error = json!({ "level": "error", "spans": [span("C:/app/src/api.rs", 0, 5)], "children": [] });
        assert_eq!(parts(&error, &pieces), [Part::HookPoint]);
    }

    #[test]
    fn errors_are_matched_to_the_pieces_they_point_at() {
        let pieces = [
            located(100..200, "api::login", Part::Shim),
            located(300..400, "api::login", Part::HookPoint),
            located(500..600, "api::logout", Part::Shim),
        ];
        let error = |spans: Vec<Value>| json!({ "level": "error", "spans": spans, "children": [] }).to_string();
        let stderr = [
            // In a shim, named the way rustc names a remapped file.
            error(vec![span(r"C:\app\src\api.rs", 150, 160)]),
            // Inside a macro's expansion, whose call site is the hook point.
            error(vec![json!({
                "file_name": "C:/registry/web-macros/src/lib.rs", "byte_start": 5, "byte_end": 9,
                "expansion": { "span": span("C:/app/src/api.rs", 310, 320), "def_site_span": null }
            })]),
            // The app's own: no piece there.
            error(vec![span("C:/app/src/api.rs", 700, 710)]),
            json!({ "level": "warning", "spans": [span("C:/app/src/api.rs", 550, 560)], "children": [] }).to_string(),
            "not json".to_owned(),
        ]
        .join("\n");
        let blame = blame(&stderr, &pieces);
        let found: Vec<(&str, Part)> = blame.pieces.iter().map(|piece| (piece.function.as_str(), piece.part)).collect();
        assert_eq!(found, [("api::login", Part::Shim), ("api::login", Part::HookPoint)]);
        assert_eq!(blame.unexplained, 1);
    }

    #[test]
    fn what_was_dropped_is_kept_for_the_same_generator_only() {
        let path = std::env::temp_dir().join(format!("scry-dropped-test-{}", std::process::id()));
        let mut dropped = Dropped::default();
        dropped.pieces.insert(Piece::new(&["api".into()], "login", Part::Shim));
        dropped.pieces.insert(Piece::new(&[], "Server::start", Part::HookPoint));
        dropped.save(&path, "generator 1");
        assert_eq!(Dropped::load(&path, "generator 1"), dropped);
        assert_eq!(Dropped::load(&path, "generator 2"), Dropped::default());
        Dropped::default().save(&path, "generator 1");
        assert!(!path.exists());
    }
}
