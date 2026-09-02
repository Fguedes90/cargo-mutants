// Copyright 2021 - 2025 Martin Pool

//! Mutations to source files, and inference of interesting mutations to apply.

use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use console::{StyledObject, style};
use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};
use similar::TextDiff;
use tracing::trace;

use crate::MUTATION_MARKER_COMMENT;
use crate::build_dir::BuildDir;
use crate::output::clean_filename;
use crate::source::SourceFile;
use crate::span::Span;

/// Unchanged lines shown on each side of a change in the generated diffs.
const DIFF_CONTEXT_LINES: usize = 8;

/// Various broad categories of mutants.
#[derive(Clone, Eq, PartialEq, Debug, Serialize)]
pub enum Genre {
    /// Replace the body of a function with a fixed value.
    FnValue,
    /// Replace `==` with `!=` and so on.
    BinaryOperator,
    UnaryOperator,
    /// Delete match arm.
    MatchArm,
    /// Replace the expression of a match arm guard with a fixed value.
    MatchArmGuard,
    /// Replace the condition of an `if` expression with a fixed value.
    IfCondition,
    /// Replace the condition of a `while` expression with `false`.
    WhileCondition,
    /// Replace a `true` literal with `false`, or vice versa.
    BoolLiteral,
    /// Delete a statement whose value is discarded.
    DeleteStatement,
    /// Replace the value of an explicit `return` with a fixed value.
    ReturnValue,
    /// Delete a field from a struct literal that has a base (default) expression.
    StructField,
}

/// The target of a mutation, providing additional context about what is being mutated.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum MutationTarget {
    /// A field in a struct literal expression.
    StructLiteralField {
        /// The name of the field being deleted.
        field_name: String,
        /// The name/type of the struct.
        struct_name: String,
    },
}

/// How one part of a mutant's description is coloured.
///
/// See [`Mutant::describe_parts`].
#[derive(Clone, Copy)]
enum PartStyle {
    Plain,
    Yellow,
    BrightYellow,
    Magenta,
    BrightMagenta,
}

/// A mutation applied to source code.
#[derive(Clone)]
pub struct Mutant {
    /// The human-readable name of this mutant: file, line/column, and change
    /// description, as returned by `name(true)`.
    ///
    /// Built on demand and cached: a name is needed for output and for the
    /// name filters, but a candidate that is discarded by
    /// `#[mutants::exclude_re]` or by an unset filter never needs one.
    name: OnceLock<String>,

    /// The change this mutant makes, without the file location: the part of
    /// `name` after the path.
    ///
    /// Cached because building it walks the styled parts of the mutant, which
    /// is the expensive half of forming a name, and both `name` and
    /// `name(false)` need exactly the same text.
    change_description: OnceLock<String>,

    /// The unified diff of this mutant against the original file.
    ///
    /// Cached because it is needed both when `mutants.json` is written, before
    /// any mutant is tested, and again when the mutant is applied; computing
    /// it diffs the whole file.
    diff: OnceLock<String>,

    /// Which file is being mutated.
    pub source_file: SourceFile,

    /// The function that's being mutated: the nearest enclosing function, if they are nested.
    ///
    /// There may be none for mutants in e.g. top-level const expressions.
    pub function: Option<Arc<Function>>,

    /// The location of the mutated textual region in the original source file.
    ///
    /// This is deleted and replaced with the replacement text.
    ///
    /// This may be long, for example when a whole function body is replaced. This is used primarily to
    /// show the line/col location of the mutation.
    pub span: Span,

    /// A shorter version of the text being replaced.
    ///
    /// For example, when a match arm is replaced, this gives only the match pattern, not the
    /// body of the arm.
    pub short_replaced: Option<String>,

    /// The replacement text.
    pub replacement: String,

    /// What general category of mutant this is.
    pub genre: Genre,

    /// Additional context about what is being mutated.
    ///
    /// This provides structured information about the mutation target, rather than
    /// encoding it in strings that need to be parsed.
    pub target: Option<MutationTarget>,

    /// Whether this mutant lives inside a construct the compiler evaluates
    /// at compile time: a `const`/`static` initializer, a `const fn` body,
    /// an array-length expression, or a const generic argument.
    ///
    /// Such positions have no coverage counter (`-Cinstrument-coverage`
    /// reaches only code the compiled program executes at runtime), even
    /// though a test can still catch the mutant by asserting on the
    /// resulting value. `--skip-uncovered` reads this to avoid skipping a
    /// mutant it cannot truthfully call uncovered.
    ///
    /// This is derived, deterministic metadata about *where* the mutant is,
    /// not part of *what change* it makes, so it is excluded from
    /// [`PartialEq`], [`fmt::Debug`], and [`Serialize`]: two mutants that
    /// make the same change at the same place are still the same mutant,
    /// and `mutants.json`/`--list --json` must not change shape.
    pub const_eval: bool,
}

/// Two mutants are the same if they make the same change to the same place:
/// the lazily-built caches are derived data, so whether they happen to be
/// filled must not affect equality.
impl PartialEq for Mutant {
    fn eq(&self, other: &Self) -> bool {
        self.source_file == other.source_file
            && self.function == other.function
            && self.span == other.span
            && self.short_replaced == other.short_replaced
            && self.replacement == other.replacement
            && self.genre == other.genre
            && self.target == other.target
    }
}

impl Eq for Mutant {}

/// The debug form deliberately omits the lazily-built name, description, and
/// diff: they are derived, and the diff would drag the whole source file into
/// the output.
#[allow(clippy::missing_fields_in_debug)] // intentional; see above
impl fmt::Debug for Mutant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutant")
            .field("source_file", &self.source_file)
            .field("function", &self.function)
            .field("span", &self.span)
            .field("short_replaced", &self.short_replaced)
            .field("replacement", &self.replacement)
            .field("genre", &self.genre)
            .field("target", &self.target)
            .field("const_eval", &self.const_eval)
            .finish()
    }
}

/// The function containing a mutant.
///
/// This is used for both mutations of the whole function, and smaller mutations within it.
#[derive(Eq, PartialEq, Debug, Serialize)]
pub struct Function {
    /// The function that's being mutated, including any containing namespaces.
    #[allow(clippy::struct_field_names)]
    pub function_name: String,

    /// The return type of the function, including a leading "-> ", as a fragment of Rust syntax.
    ///
    /// Empty if the function has no return type (i.e. returns `()`).
    pub return_type: String,

    /// The span (line/column range) of the entire function.
    pub span: Span,
}

impl Mutant {
    /// Construct a mutant discovered while walking source code.
    ///
    /// The human-readable name and the diff are not built here: they are
    /// computed on first use and cached, so a candidate that is discarded by
    /// a filter costs nothing to name.
    // All of it is discovered separately while walking the AST, and grouping
    // parts of it into a struct would only move the argument list.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_discovered(
        source_file: SourceFile,
        function: Option<Arc<Function>>,
        span: Span,
        short_replaced: Option<String>,
        replacement: String,
        genre: Genre,
        target: Option<MutationTarget>,
        const_eval: bool,
    ) -> Self {
        Mutant {
            name: OnceLock::new(),
            change_description: OnceLock::new(),
            diff: OnceLock::new(),
            source_file,
            function,
            span,
            short_replaced,
            replacement,
            genre,
            target,
            const_eval,
        }
    }

    /// The replacement text as it is written into the mutated file, including
    /// the marker comment.
    ///
    /// Shared by [`Self::mutated_code`] and by diff generation, so that the
    /// diff always describes exactly what would be written to disk.
    fn replacement_with_marker(&self) -> String {
        format!("{} {}", self.replacement, MUTATION_MARKER_COMMENT)
    }

    /// Return text of the whole file with the mutation applied.
    ///
    /// The replacement is followed by [`MUTATION_MARKER_COMMENT`]. Every genre
    /// builds its replacement by printing a `TokenStream`, which can't contain
    /// a comment, so the marker can't be swallowed by an unterminated comment
    /// in the replacement; the assertion below holds any new genre to that.
    pub fn mutated_code(&self) -> String {
        debug_assert!(
            !self.replacement.contains("/*") && !self.replacement.contains("//"),
            "replacement {:?} opens a comment, which would swallow the mutation marker",
            self.replacement
        );
        self.span.replace_indexed(
            self.source_file.code(),
            self.source_file.line_index(),
            &self.replacement_with_marker(),
        )
    }

    /// Describe the mutant briefly, not including the location.
    ///
    /// The result is like `replace factorial -> u32 with Default::default()`.
    pub fn describe_change(&self) -> &str {
        self.change_description.get_or_init(|| {
            let mut out = String::new();
            self.describe_parts(&mut |_style, text| out.push_str(text));
            out
        })
    }

    /// The full name of this mutant, including the line and column.
    ///
    /// This is the name used by the name filters, by `mutants.json`, and by
    /// the caught/missed list files, so it is cached.
    pub fn full_name(&self) -> &str {
        self.name.get_or_init(|| {
            format!(
                "{path}:{line}:{col}: {description}",
                path = self.source_file.tree_relative_slashes(),
                line = self.span.start.line,
                col = self.span.start.column,
                description = self.describe_change(),
            )
        })
    }

    /// Append the name of this mutant to `out`.
    ///
    /// Equivalent to `out.push_str(&self.name(show_line_col))` but without
    /// allocating the name first, which `--list` would otherwise do once per
    /// mutant.
    pub fn write_name(&self, show_line_col: bool, out: &mut String) {
        if show_line_col {
            out.push_str(self.full_name());
        } else {
            out.push_str(self.source_file.tree_relative_slashes());
            out.push_str(": ");
            out.push_str(self.describe_change());
        }
    }

    pub fn name(&self, show_line_col: bool) -> String {
        let mut out = String::new();
        self.write_name(show_line_col, &mut out);
        out
    }

    /// Return a one-line description of this mutant, with coloring, including the file names
    /// and optionally the line and column.
    pub fn to_styled_string(&self, show_line_col: bool) -> String {
        let mut v = vec![self.source_file.tree_relative_slashes().to_owned()];
        if show_line_col {
            v.push(format!(
                ":{}:{}",
                self.span.start.line, self.span.start.column
            ));
        }
        v.push(": ".to_owned());
        v.extend(self.styled_parts().into_iter().map(|x| x.to_string()));
        v.join("")
    }

    /// Emit the parts of this mutant's description, in order, to `f`.
    ///
    /// This is the single definition of the description text; the plain
    /// rendering concatenates the parts and the coloured rendering styles them
    /// according to [`PartStyle`], so the two cannot drift apart. Parts are
    /// passed as `&str` so that the plain rendering, which is the common case
    /// and is built for every mutant, allocates only its output string.
    fn describe_parts(&self, f: &mut impl FnMut(PartStyle, &str)) {
        match self.genre {
            Genre::FnValue => {
                f(PartStyle::Plain, "replace ");
                let function = self
                    .function
                    .as_ref()
                    .expect("FnValue mutant should have a function");
                f(PartStyle::BrightMagenta, &function.function_name);
                if !function.return_type.is_empty() {
                    f(PartStyle::Plain, " ");
                    f(PartStyle::Magenta, &function.return_type);
                }
                f(PartStyle::Plain, " with ");
                f(PartStyle::Yellow, self.replacement_text());
            }
            Genre::MatchArmGuard => {
                f(PartStyle::Plain, "replace match guard ");
                let original = self.original_text();
                f(PartStyle::Yellow, &squash_lines(&original));
                f(PartStyle::Plain, " with ");
                f(PartStyle::Yellow, self.replacement_text());
            }
            Genre::MatchArm => {
                f(PartStyle::Plain, "delete match arm ");
                f(
                    PartStyle::Yellow,
                    &squash_lines(
                        self.short_replaced
                            .as_ref()
                            .expect("short_replaced should be set on MatchArm"),
                    ),
                );
            }
            Genre::StructField => {
                if let Some(MutationTarget::StructLiteralField {
                    field_name,
                    struct_name,
                }) = &self.target
                {
                    f(PartStyle::Plain, "delete field ");
                    f(PartStyle::Yellow, field_name);
                    f(PartStyle::Plain, " from struct ");
                    f(PartStyle::Yellow, struct_name);
                    f(PartStyle::Plain, " expression");
                } else {
                    // Fallback: shouldn't happen with proper initialization
                    f(PartStyle::Plain, "delete field from struct expression");
                }
            }
            _ => {
                if self.replacement.is_empty() {
                    f(PartStyle::Plain, "delete ");
                } else {
                    f(PartStyle::Plain, "replace ");
                }
                f(PartStyle::Yellow, &self.original_text());
                if !self.replacement.is_empty() {
                    f(PartStyle::Plain, " with ");
                    f(PartStyle::BrightYellow, &self.replacement);
                }
            }
        }
        if !matches!(self.genre, Genre::FnValue)
            && let Some(func) = &self.function
        {
            f(PartStyle::Plain, " in ");
            f(PartStyle::BrightMagenta, &func.function_name);
        }
    }

    fn styled_parts(&self) -> Vec<StyledObject<String>> {
        // This is like `impl Display for Mutant`, but with colors.
        // The text content is the same: see `describe_parts`.
        let mut v: Vec<StyledObject<String>> = Vec::new();
        self.describe_parts(&mut |part_style, text| {
            let styled = style(text.to_owned());
            v.push(match part_style {
                PartStyle::Plain => styled,
                PartStyle::Yellow => styled.yellow(),
                PartStyle::BrightYellow => styled.bright().yellow(),
                PartStyle::Magenta => styled.magenta(),
                PartStyle::BrightMagenta => styled.bright().magenta(),
            });
        });
        v
    }

    pub fn original_text(&self) -> String {
        self.span
            .extract_indexed(self.source_file.code(), self.source_file.line_index())
    }

    /// Return the text inserted for this mutation.
    pub fn replacement_text(&self) -> &str {
        self.replacement.as_str()
    }

    /// Return a unified diff for the mutant.
    ///
    /// Only the neighbourhood of the mutated span is diffed. Diffing whole
    /// files costs O(file) per mutant — `similar` tokenizes and hashes every
    /// line of both sides — and `mutants.json` carries a diff for every
    /// mutant, so a large file used to cost O(mutants x file size) before a
    /// single test was run. Lines further than the context radius from the
    /// span are identical on both sides and so cannot appear in the output;
    /// the hunk headers are renumbered afterwards to the positions a
    /// whole-file diff would have reported.
    ///
    /// The mutated side is built for the window alone, so the whole mutated
    /// file is never materialised just to be diffed.
    fn diff(&self) -> String {
        let orig = self.source_file.code();
        let line_index = self.source_file.line_index();
        let (lo, hi) = self.span.byte_range(orig, line_index);
        let replacement = self.replacement_with_marker();

        let first_line = self
            .span
            .start
            .line
            .saturating_sub(DIFF_CONTEXT_LINES)
            .max(1);
        // A window that doesn't contain the span would give a wrong diff, so
        // fall back to the whole file rather than trust inconsistent input.
        match line_index
            .line_start(first_line)
            .filter(|&win_lo| win_lo <= lo)
        {
            Some(win_lo) => {
                let win_hi = line_index
                    .line_start(self.span.end.line + DIFF_CONTEXT_LINES + 1)
                    .unwrap_or(orig.len())
                    .max(hi);
                let mut new_window = String::with_capacity(win_hi - win_lo + replacement.len());
                new_window.push_str(&orig[win_lo..lo]);
                new_window.push_str(&replacement);
                new_window.push_str(&orig[hi..win_hi]);
                let diff = self.unified_diff(&orig[win_lo..win_hi], &new_window);
                renumber_hunks(&diff, first_line - 1)
            }
            None => self.unified_diff(orig, &self.mutated_code()),
        }
    }

    fn unified_diff(&self, old: &str, new: &str) -> String {
        // There shouldn't be any newlines, but just in case...
        let new_label = self.describe_change().replace('\n', " ");
        TextDiff::from_lines(old, new)
            .unified_diff()
            .context_radius(DIFF_CONTEXT_LINES)
            .header(self.source_file.tree_relative_slashes(), &new_label)
            .to_string()
    }

    /// Return the unified diff for the mutant, computing it at most once.
    ///
    /// `mutants.json` is written for every mutant before any of them is
    /// tested, and the same diff is then written into the mutant's own output
    /// directory when it is applied, so the diff of every tested mutant would
    /// otherwise be computed twice.
    pub fn cached_diff(&self) -> &str {
        self.diff.get_or_init(|| self.diff())
    }

    /// Apply this mutant to the relevant file within a `BuildDir`.
    pub fn apply(&self, build_dir: &BuildDir, mutated_code: &str) -> Result<()> {
        trace!(?self, "Apply mutant");
        build_dir.overwrite_file(&self.source_file.tree_relative_path, mutated_code)
    }

    pub fn revert(&self, build_dir: &BuildDir) -> Result<()> {
        trace!(?self, "Revert mutant");
        build_dir.overwrite_file(
            &self.source_file.tree_relative_path,
            self.source_file.code(),
        )
    }

    /// Return a string describing this mutant that's suitable for building a log file name,
    /// but can contain slashes.
    pub fn log_file_name_base(&self) -> String {
        // TODO: Also include a unique number so that they can't collide, even
        // with similar mutants on the same line?
        format!(
            "{filename}_line_{line}_col_{col}",
            filename = clean_filename(self.source_file.tree_relative_slashes()),
            line = self.span.start.line,
            col = self.span.start.column,
        )
    }

    /// Convert this mutant to a JSON value including the diff.
    ///
    /// This is used for both `--list --json` output and for writing `mutants.out/mutants.json`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::to_value(self).expect("Serialize mutant");
        obj.as_object_mut()
            .unwrap()
            .insert("diff".to_owned(), serde_json::json!(self.cached_diff()));
        obj
    }
}

impl Serialize for Mutant {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // custom serialize to omit inessential info
        let mut ss = serializer.serialize_struct("Mutant", 7)?;
        ss.serialize_field("name", &self.full_name())?;
        ss.serialize_field("package", &self.source_file.package.name)?;
        ss.serialize_field("file", &self.source_file.tree_relative_slashes())?;
        ss.serialize_field("function", &self.function.as_ref().map(Arc::as_ref))?;
        ss.serialize_field("span", &self.span)?;
        ss.serialize_field("replacement", &self.replacement)?;
        ss.serialize_field("genre", &self.genre)?;
        ss.end()
    }
}

/// Shift the line numbers in every `@@ -a,b +c,d @@` header by `offset`.
///
/// The diff was computed over a window of the file, so its hunks are numbered
/// from the start of that window. This restores the numbering that a
/// whole-file diff would have produced. Anything that doesn't parse as a hunk
/// header is passed through untouched, so a body line that happens to start
/// with `@@ -` cannot be corrupted into something else.
fn renumber_hunks(diff: &str, offset: usize) -> String {
    if offset == 0 {
        return diff.to_owned();
    }
    let mut out = String::with_capacity(diff.len());
    for line in diff.split_inclusive('\n') {
        match renumber_hunk_header(line, offset) {
            Some(header) => out.push_str(&header),
            None => out.push_str(line),
        }
    }
    out
}

/// Rewrite one `@@ -a,b +c,d @@` header, or `None` if `line` isn't one.
fn renumber_hunk_header(line: &str, offset: usize) -> Option<String> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, tail) = rest.split_once(" @@")?;
    let old = shift_hunk_range(old, offset)?;
    let new = shift_hunk_range(new, offset)?;
    Some(format!("@@ -{old} +{new} @@{tail}"))
}

/// Add `offset` to the start line of an `a,b` (or bare `a`) hunk range.
///
/// A start of 0, which unified diff uses for an empty side, is left alone:
/// there is no line 0 to move.
fn shift_hunk_range(range: &str, offset: usize) -> Option<String> {
    let (start, count) = match range.split_once(',') {
        Some((start, count)) => (start, Some(count)),
        None => (range, None),
    };
    let start: usize = start.parse().ok()?;
    let start = if start == 0 { 0 } else { start + offset };
    Some(match count {
        Some(count) => format!("{start},{count}"),
        None => start.to_string(),
    })
}

/// Combine multiple lines to one, removing indentation following a newline.
///
/// Newlines are replaced by a space, only if there is not already a trailing space.
pub fn squash_lines(s: &str) -> Cow<'_, str> {
    if s.contains('\n') {
        let mut r = String::new();
        let mut in_indent = false;
        for c in s.chars() {
            match c {
                ' ' | '\t' | '\n' if in_indent => (),
                '\n' => {
                    if !r.ends_with(' ') {
                        r.push(' ');
                    }
                    in_indent = true;
                }
                c => {
                    in_indent = false;
                    r.push(c);
                }
            }
        }
        Cow::Owned(r)
    } else {
        Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod test {
    use indoc::indoc;
    use itertools::Itertools;
    use pretty_assertions::assert_eq;

    use crate::test_util::copy_of_testdata;
    use crate::visit::mutate_source_str;
    use crate::*;

    #[test]
    fn squash_lines() {
        use super::squash_lines;
        assert_eq!(squash_lines("squash_lines a b c"), "squash_lines a b c");
        assert_eq!(squash_lines("a\n    b c \n\nd  \n  e"), "a b c d  e");
    }

    #[test]
    fn discover_factorial_mutants() {
        let tmp = copy_of_testdata("factorial");
        let workspace = Workspace::open(tmp.path()).unwrap();
        let options = Options::default();
        let mutants = workspace
            .discover(&PackageFilter::All, &options, &Console::new())
            .unwrap()
            .mutants;
        assert_eq!(mutants.len(), 5);

        // Some checks about fields that we expect in the debug format, without being too brittle
        let debug_format = format!("{:#?}", mutants[0]);
        println!("mutants[0]: {debug_format}");
        assert!(debug_format.contains("Mutant {"));
        assert!(debug_format.contains("function: Some("));
        assert!(debug_format.contains(r#"replacement: "()""#));
        assert!(debug_format.contains("genre: FnValue"));
        assert!(debug_format.contains("span: Span(2, 5, 4, 6)"));
        assert!(debug_format.contains("short_replaced: None"));
        assert!(debug_format.contains(r#"name: "cargo-mutants-testdata-factorial""#));
        assert!(
            debug_format.contains(r#""src/bin/factorial.rs""#)
                || debug_format.contains(r#""src\\bin\\factorial.rs""#) // backslashes escaped in string debug form
        );
        assert!(
            !debug_format.contains("fn main()"),
            "Debug form seems to contain source code"
        );
        assert!(
            debug_format.len() < 800,
            "Debug form seems to be too long: {} bytes",
            debug_format.len()
        );

        assert_eq!(
            mutants[0].name(true),
            "src/bin/factorial.rs:2:5: replace main with ()"
        );

        println!("mutants[1]: {:#?}", mutants[1]);
        assert_eq!(
            mutants[1].source_file.package.name,
            "cargo-mutants-testdata-factorial"
        );
        assert_eq!(
            mutants[1].function.as_ref().unwrap().function_name,
            "factorial"
        );
        assert_eq!(mutants[1].function.as_ref().unwrap().return_type, "-> u32");
        assert_eq!(mutants[1].genre, Genre::FnValue);
        assert_eq!(mutants[1].replacement, "0");
        assert_eq!(
            mutants[1].name(false),
            "src/bin/factorial.rs: replace factorial -> u32 with 0"
        );
        assert_eq!(
            mutants[1].name(true),
            "src/bin/factorial.rs:8:5: replace factorial -> u32 with 0"
        );
        assert_eq!(
            mutants[2].name(true),
            "src/bin/factorial.rs:8:5: replace factorial -> u32 with 1"
        );
    }

    #[test]
    fn filter_by_attributes() {
        let tmp = copy_of_testdata("hang_avoided_by_attr");
        let mutants = Workspace::open(tmp.path())
            .unwrap()
            .discover(&PackageFilter::All, &Options::default(), &Console::new())
            .unwrap()
            .mutants;
        let descriptions = mutants.iter().map(Mutant::describe_change).collect_vec();
        assert_eq!(
            descriptions,
            [
                "replace controlled_loop with ()",
                "replace should_stop() with true in controlled_loop",
                "replace start.elapsed() > Duration::from_secs(60 * 5) with true in controlled_loop",
                "replace start.elapsed() > Duration::from_secs(60 * 5) with false in controlled_loop",
                "replace > with == in controlled_loop",
                "replace > with < in controlled_loop",
                "replace > with >= in controlled_loop",
                "replace * with + in controlled_loop",
                "replace * with / in controlled_loop",
            ]
        );
    }

    #[test]
    fn always_skip_constructors_called_new() {
        let code = indoc! { r"
            struct S {
                x: i32,
            }

            impl S {
                fn new(x: i32) -> Self {
                    Self { x }
                }
            }
        " };
        let mutants = mutate_source_str(code, &Options::default()).unwrap();
        assert_eq!(mutants, []);
    }

    #[test]
    fn mutate_factorial() -> Result<()> {
        let temp = copy_of_testdata("factorial");
        let tree_path = temp.path();
        let mutants = Workspace::open(tree_path)?
            .discover(&PackageFilter::All, &Options::default(), &Console::new())?
            .mutants;
        assert_eq!(mutants.len(), 5);

        let mutated_code = mutants[0].mutated_code();
        assert_eq!(mutants[0].function.as_ref().unwrap().function_name, "main");
        assert_eq!(
            strip_trailing_space(&mutated_code),
            indoc! { r#"
                fn main() {
                    () /* ~ changed by cargo-mutants ~ */
                }

                fn factorial(n: u32) -> u32 {
                    let mut a = 1;
                    for i in 2..=n {
                        a *= i;
                    }
                    a
                }

                #[test]
                fn test_factorial() {
                    println!("factorial({}) = {}", 6, factorial(6)); // This line is here so we can see it in --nocapture
                    assert_eq!(factorial(6), 720);
                }
                "#
            }
        );

        let mutated_code = mutants[1].mutated_code();
        assert_eq!(
            mutants[1].function.as_ref().unwrap().function_name,
            "factorial"
        );
        assert_eq!(
            strip_trailing_space(&mutated_code),
            indoc! { r#"
                fn main() {
                    for i in 1..=6 {
                        println!("{}! = {}", i, factorial(i));
                    }
                }

                fn factorial(n: u32) -> u32 {
                    0 /* ~ changed by cargo-mutants ~ */
                }

                #[test]
                fn test_factorial() {
                    println!("factorial({}) = {}", 6, factorial(6)); // This line is here so we can see it in --nocapture
                    assert_eq!(factorial(6), 720);
                }
                "#
            }
        );
        Ok(())
    }

    fn strip_trailing_space(s: &str) -> String {
        // Split on \n so that we retain empty lines etc
        s.split('\n').map(str::trim_end).join("\n")
    }
}
