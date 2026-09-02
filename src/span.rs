// Copyright 2021-2023 Martin Pool

//! Locations (line/column) and spans between them in source code.
//!
//! This is similar to, and can be automatically derived from,
//! `proc_macro2::Span` and `proc_macro2::LineColumn`, but is
//! a bit more convenient for our purposes.

use std::fmt;

use serde::Serialize;

/// A (line, column) position in a source file.
#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
pub struct LineColumn {
    /// 1-based line number.
    pub line: usize,

    /// 1-based column, measured in chars.
    pub column: usize,
}

impl From<proc_macro2::LineColumn> for LineColumn {
    fn from(l: proc_macro2::LineColumn) -> Self {
        LineColumn {
            line: l.line,
            column: l.column + 1,
        }
    }
}

impl fmt::Debug for LineColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LineColumn({}, {})", self.line, self.column)
    }
}

/// A contiguous text span in a file.
#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
pub struct Span {
    /// The *inclusive* position where the span starts.
    pub start: LineColumn,
    /// The *exclusive* position where the span ends.
    pub end: LineColumn,
}

/// Byte offsets of the start of every line in a string.
///
/// Resolving a [`LineColumn`] to a byte offset otherwise requires scanning from
/// the start of the string. Doing that once per mutant makes discovery
/// quadratic in file size, so callers that resolve many spans within one file
/// build this once and share it.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LineIndex {
    /// Byte offset of the first character of each line, in order.
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(s: &str) -> LineIndex {
        let mut line_starts = Vec::new();
        line_starts.push(0);
        line_starts.extend(s.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { line_starts }
    }

    /// Byte offset of the first character at or after `pos`.
    ///
    /// Columns count characters rather than bytes, and a `\r` shares the column
    /// of the character that follows it, matching how positions are counted
    /// when they are derived from `proc_macro2`. A position beyond the end of
    /// its line resolves to the start of the next line, and one beyond the end
    /// of the string resolves to its length.
    fn byte_offset(&self, s: &str, pos: LineColumn) -> usize {
        let Some(line) = pos.line.checked_sub(1) else {
            return 0;
        };
        let Some(&line_start) = self.line_starts.get(line) else {
            return s.len();
        };
        let line_end = self.line_starts.get(line + 1).copied().unwrap_or(s.len());
        let mut column = 1;
        for (offset, c) in s[line_start..line_end].char_indices() {
            if column >= pos.column {
                return line_start + offset;
            }
            if c != '\r' {
                column += 1;
            }
        }
        line_end
    }
}

impl Span {
    #[allow(dead_code)]
    pub fn quad(
        start_line: usize,
        start_column: usize,
        end_line: usize,
        end_column: usize,
    ) -> Self {
        Span {
            start: LineColumn {
                line: start_line,
                column: start_column,
            },
            end: LineColumn {
                line: end_line,
                column: end_column,
            },
        }
    }

    /// Return the region of a multi-line string that this span covers, using a
    /// prebuilt line index.
    pub fn extract_indexed(&self, s: &str, line_index: &LineIndex) -> String {
        let (start, end) = self.byte_range(s, line_index);
        s[start..end].to_owned()
    }

    /// Replace a subregion of text, using a prebuilt line index.
    pub fn replace_indexed(&self, s: &str, line_index: &LineIndex, replacement: &str) -> String {
        let (start, end) = self.byte_range(s, line_index);
        let mut r = String::with_capacity(s.len() - (end - start) + replacement.len());
        r.push_str(&s[..start]);
        r.push_str(replacement);
        r.push_str(&s[end..]);
        r
    }

    /// Resolve this span to a byte range within `s`.
    ///
    /// The end is clamped to the start so that a reversed or degenerate span
    /// yields an empty range rather than panicking.
    fn byte_range(&self, s: &str, line_index: &LineIndex) -> (usize, usize) {
        let start = line_index.byte_offset(s, self.start);
        let end = line_index.byte_offset(s, self.end).max(start);
        (start, end)
    }
}

/// Single-span convenience wrappers.
///
/// Production code extracts many spans from one file and shares a [`LineIndex`]
/// to keep that linear, so these one-shot forms are only used by tests.
#[cfg(test)]
impl Span {
    /// Return the region of a multi-line string that this span covers.
    pub fn extract(&self, s: &str) -> String {
        self.extract_indexed(s, &LineIndex::new(s))
    }

    /// Returns a copy of `s` with the region identified by this span replaced
    /// by `replacement`.
    pub fn replace(&self, s: &str, replacement: &str) -> String {
        self.replace_indexed(s, &LineIndex::new(s), replacement)
    }
}

impl From<proc_macro2::Span> for Span {
    fn from(s: proc_macro2::Span) -> Self {
        Span {
            start: s.start().into(),
            end: s.end().into(),
        }
    }
}

impl From<&proc_macro2::Span> for Span {
    fn from(s: &proc_macro2::Span) -> Self {
        Span {
            start: s.start().into(),
            end: s.end().into(),
        }
    }
}

impl From<proc_macro2::extra::DelimSpan> for Span {
    fn from(s: proc_macro2::extra::DelimSpan) -> Self {
        // Get the span for the whole block from the start delimiter
        // to the end.
        let joined = s.join();
        Span {
            start: joined.start().into(),
            end: joined.end().into(),
        }
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A concise form, similar to ::quad
        write!(
            f,
            "Span({}, {}, {}, {})",
            self.start.line, self.start.column, self.end.line, self.end.column
        )
    }
}

#[cfg(test)]
mod test {
    use indoc::indoc;
    // use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn linecolumn_debug_form() {
        let lc = LineColumn { line: 1, column: 2 };
        assert_eq!(format!("{lc:?}"), "LineColumn(1, 2)");
    }

    #[test]
    fn span_debug_form() {
        let span = Span::quad(1, 2, 3, 4);
        assert_eq!(format!("{span:?}"), "Span(1, 2, 3, 4)");
    }

    #[test]
    fn cut_before_crlf() {
        let source = "fn foo() {\r\n    wibble();\r\n}\r\n//hey!\r\n";
        let span = Span::quad(1, 10, 3, 2);
        assert_eq!(span.extract(source), "{\r\n    wibble();\r\n}");
        assert_eq!(span.replace(source, "{}"), "fn foo() {}\r\n//hey!\r\n");
    }

    #[test]
    fn empty_span_in_empty_string() {
        let span = Span::quad(1, 1, 1, 1);
        assert_eq!(span.extract(""), "");
        assert_eq!(span.replace("", "x"), "x");
    }

    #[test]
    fn empty_span_at_start_of_string() {
        let span = Span::quad(1, 1, 1, 1);
        assert_eq!(span.extract("hello"), "");
        assert_eq!(span.replace("hello", "x"), "xhello");
    }

    #[test]
    fn empty_span_at_end_of_string() {
        let span = Span::quad(1, 6, 1, 6);
        assert_eq!(span.extract("hello"), "");
        assert_eq!(span.replace("hello", "x"), "hellox");
    }

    #[test]
    fn cut_including_crlf() {
        let source = "fn foo() {\r\n    wibble();\r\n}\r\n//hey!\r\n";
        let span = Span::quad(1, 10, 3, 3);
        assert_eq!(span.extract(source), "{\r\n    wibble();\r\n}\r\n");
        assert_eq!(span.replace(source, "{}\r\n"), "fn foo() {}\r\n//hey!\r\n");
    }
    #[test]
    fn span_ops() {
        let source = indoc! { r"

            fn foo() {
                some();
                stuff();
            }

            const BAR: u32 = 32;
        " };
        // typical multi-line case
        let span = Span::quad(2, 10, 5, 2);
        assert_eq!(span.extract(source), "{\n    some();\n    stuff();\n}");
        let replaced = span.replace(source, "{ /* body deleted */ }");
        assert_eq!(
            replaced,
            indoc! { r"

                fn foo() { /* body deleted */ }

                const BAR: u32 = 32;
            " }
        );

        // single-line case
        let span = Span::quad(7, 18, 7, 20);
        assert_eq!(span.extract(source), "32");
        let replaced = span.replace(source, "69");
        assert_eq!(
            replaced,
            indoc! { r"

                fn foo() {
                    some();
                    stuff();
                }

                const BAR: u32 = 69;
            " }
        );
    }
}
