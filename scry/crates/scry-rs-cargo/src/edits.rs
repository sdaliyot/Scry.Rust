//! Text edits placed by token position, applied to the original source in one pass.
//!
//! Every edit is an insertion on an existing line or a replacement within one, never a new line,
//! so the rewritten file keeps the original's line numbers: compiler errors, panics and the
//! debugger point at the right lines of the original files.

use proc_macro2::{LineColumn, Span};

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
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

impl Edits {
    pub fn insert(&mut self, at: usize, text: impl Into<String>) {
        self.list.push(Edit { start: at, end: at, text: text.into() });
    }

    pub fn replace(&mut self, start: usize, end: usize, text: impl Into<String>) {
        self.list.push(Edit { start, end, text: text.into() });
    }

    /// Insertions at the same offset keep the order they were added in.
    pub fn apply(self, text: &str) -> String {
        // A newline in generated text would shift every line after it.
        debug_assert!(self.list.iter().all(|edit| !edit.text.contains('\n')), "an edit adds a line");
        let mut order: Vec<usize> = (0..self.list.len()).collect();
        order.sort_by_key(|&index| (self.list[index].start, index));
        let mut result = text.to_owned();
        for &index in order.iter().rev() {
            let edit = &self.list[index];
            result.replace_range(edit.start..edit.end, &edit.text);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_insertion_order_at_one_offset() {
        let mut edits = Edits::default();
        edits.insert(4, "a");
        edits.insert(4, "b");
        edits.replace(0, 3, "XYZ");
        assert_eq!(edits.apply("abc def"), "XYZ abdef");
    }

    #[test]
    fn maps_columns_as_characters() {
        let source = Source::new("é = 1;\nlet x;".to_owned());
        assert_eq!(source.offset(LineColumn { line: 1, column: 2 }), 3);
        assert_eq!(source.offset(LineColumn { line: 2, column: 4 }), 12);
    }
}
