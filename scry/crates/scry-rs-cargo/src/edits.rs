//! Text edits placed by token position, applied to the original source in one pass.
//!
//! Every edit is an insertion on an existing line or a replacement within one, never a new line,
//! so the rewritten file keeps the original's line numbers: compiler errors, panics and the
//! debugger point at the right lines of the original files.

use std::ops::Range;

use proc_macro2::{LineColumn, Span};

use crate::pieces::Piece;

pub struct Source {
    pub text: String,
    line_starts: Vec<usize>,
    /// syn skips a byte-order mark, so columns on the first line start one character late.
    bom: bool,
}

impl Source {
    pub fn new(text: String) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
        let bom = text.starts_with('\u{feff}');
        Self { text, line_starts, bom }
    }

    /// The byte offset of a line and column (a character count) reported by proc-macro2.
    pub fn offset(&self, position: LineColumn) -> usize {
        let Some(&start) = self.line_starts.get(position.line.saturating_sub(1)) else {
            return self.text.len();
        };
        let column = position.column + usize::from(self.bom && position.line == 1);
        self.text[start..].char_indices().nth(column).map_or(self.text.len(), |(index, _)| start + index)
    }

    pub fn start(&self, span: Span) -> usize {
        self.offset(span.start())
    }

    pub fn end(&self, span: Span) -> usize {
        self.offset(span.end())
    }
}

#[derive(Default)]
pub struct Edits {
    list: Vec<Edit>,
    /// Stretches of the original text that belong to a piece, though not generated for it: the
    /// function a hook point sits in, where an attribute macro may report what it made of it.
    marks: Vec<(usize, usize, Piece)>,
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
    piece: Option<Piece>,
}

impl Edits {
    pub fn insert(&mut self, at: usize, text: impl Into<String>) {
        self.list.push(Edit { start: at, end: at, text: text.into(), piece: None });
    }

    /// An insertion that is (part of) `piece`, so an error inside it is blamed on the piece.
    pub fn insert_piece(&mut self, at: usize, text: impl Into<String>, piece: Piece) {
        self.list.push(Edit { start: at, end: at, text: text.into(), piece: Some(piece) });
    }

    pub fn replace(&mut self, start: usize, end: usize, text: impl Into<String>) {
        self.list.push(Edit { start, end, text: text.into(), piece: None });
    }

    /// Marks the original text from `start` to `end` as belonging to `piece` too.
    pub fn mark(&mut self, start: usize, end: usize, piece: Piece) {
        self.marks.push((start, end, piece));
    }

    /// Insertions at the same offset keep the order they were added in.
    #[cfg(test)]
    pub fn apply(self, text: &str) -> String {
        self.apply_located(text).0
    }

    /// The rewritten text, and where each piece's insertions and marks ended up in it: a range,
    /// the piece, and whether it is a mark rather than generated text.
    pub fn apply_located(self, text: &str) -> (String, Vec<(Range<usize>, Piece, bool)>) {
        // A newline in generated text would shift every line after it.
        debug_assert!(self.list.iter().all(|edit| !edit.text.contains('\n')), "an edit adds a line");
        let mut order: Vec<usize> = (0..self.list.len()).collect();
        order.sort_by_key(|&index| (self.list[index].start, index));
        let mut result =
            String::with_capacity(text.len() + self.list.iter().map(|edit| edit.text.len()).sum::<usize>());
        let mut located = Vec::new();
        // After each edit: the original offset where it ends, and how far later text has moved.
        let mut shifts: Vec<(usize, isize)> = Vec::new();
        let mut copied = 0;
        for &index in &order {
            let edit = &self.list[index];
            result.push_str(&text[copied..edit.start.max(copied)]);
            copied = copied.max(edit.end);
            let at = result.len();
            result.push_str(&edit.text);
            if let Some(piece) = &edit.piece {
                located.push((at..result.len(), piece.clone(), false));
            }
            shifts.push((copied, result.len() as isize - copied as isize));
        }
        result.push_str(&text[copied..]);
        // An offset in the original, moved by the edits before it. What is inserted at a mark's
        // start falls inside the mark, what is inserted at its end outside.
        let place = |offset: usize, inside: bool| {
            let before = |at: usize| if inside { at <= offset } else { at < offset };
            let shift = shifts.iter().rev().find(|(at, _)| before(*at)).map_or(0, |(_, shift)| *shift);
            offset.saturating_add_signed(shift)
        };
        for (start, end, piece) in self.marks {
            located.push((place(start, false)..place(end, false), piece, true));
        }
        (result, located)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pieces::Part;

    #[test]
    fn keeps_insertion_order_at_one_offset() {
        let mut edits = Edits::default();
        edits.insert(4, "a");
        edits.insert(4, "b");
        edits.replace(0, 3, "XYZ");
        assert_eq!(edits.apply("abc def"), "XYZ abdef");
    }

    #[test]
    fn locates_pieces_and_marks_in_the_rewritten_text() {
        let piece = |name: &str| Piece::new(&[], name, Part::Shim);
        let mut edits = Edits::default();
        edits.insert(0, "#[x] ");
        edits.insert_piece(12, " SHIM", piece("f"));
        edits.insert_piece(9, "HOOK ", piece("f"));
        edits.mark(3, 12, piece("body"));
        let (text, located) = edits.apply_located("fn f() { 1 }\nfn g() {}");
        assert_eq!(text, "#[x] fn f() { HOOK 1 } SHIM\nfn g() {}");
        let found: Vec<&str> = located.iter().map(|(range, _, _)| &text[range.clone()]).collect();
        assert_eq!(found, ["HOOK ", " SHIM", "f() { HOOK 1 }"]);
    }

    #[test]
    fn maps_columns_as_characters() {
        let source = Source::new("é = 1;\nlet x;".to_owned());
        assert_eq!(source.offset(LineColumn { line: 1, column: 2 }), 3);
        assert_eq!(source.offset(LineColumn { line: 2, column: 4 }), 12);
    }
}
