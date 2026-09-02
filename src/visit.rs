// Copyright 2021 - 2026 Martin Pool

//! Visit all the files in a source tree, and then the AST of each file,
//! to discover mutation opportunities.
//!
//! Walking the tree starts with some root files known to the build tool:
//! e.g. for cargo they are identified from the targets. The tree walker then
//! follows `mod` statements to recursively visit other referenced files.

#![warn(clippy::pedantic)]
#![allow(clippy::needless_raw_string_hashes)]

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Arc, mpsc};
use std::thread;
use std::vec;

use anyhow::anyhow;
use camino::{Utf8Path, Utf8PathBuf};
use proc_macro2::{Ident, TokenStream};
use quote::{ToTokens, quote};
use regex::RegexSet;
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{
    Attribute, BinOp, Block, Expr, ExprPath, File, ItemFn, ReturnType, Signature, Stmt, UnOp,
};
use tracing::{debug_span, error, info, trace, trace_span, warn};

use crate::console::WalkProgress;
use crate::fnvalue::{EnumIndex, return_type_replacements};
use crate::mutant::{Function, MutationTarget};
use crate::package::Package;
use crate::pretty::ToPrettyString;
use crate::source::SourceFile;
use crate::span::{LineColumn, Span};
use crate::{Console, Context, Genre, Mutant, Options, Result, check_interrupted};

/// Mutants and files discovered in a source tree.
///
/// Files are listed separately so that we can represent files that
/// were visited but that produced no mutants.
pub struct Discovered {
    pub mutants: Vec<Mutant>,
    pub files: Vec<SourceFile>,
}

impl Discovered {
    pub(crate) fn remove_previously_caught(&mut self, previously_caught: &[String]) {
        self.mutants.retain(|m| {
            let c = previously_caught.iter().any(|c| c == m.full_name());
            if c {
                trace!(name = m.full_name(), "skip previously caught mutant");
            }
            !c
        });
    }
}

/// Discover all mutants and all source files.
///
/// The returned `Discovered` struct contains all mutants found in the
/// source tree, and also a list of all source files visited (whether
/// they generated mutants or not).
pub fn walk_tree(
    workspace_dir: &Utf8Path,
    packages: &[Arc<Package>],
    options: &Options,
    console: &Console,
) -> Result<Discovered> {
    let mut mutants = Vec::new();
    let mut files = Vec::new();
    let progress = console.start_walk_tree();
    for package in packages {
        let (mut package_mutants, mut package_files) =
            walk_package(workspace_dir, package, &progress, options)?;
        mutants.append(&mut package_mutants);
        files.append(&mut package_files);
    }

    progress.finish();
    Ok(Discovered { mutants, files })
}

/// Walk one package, starting from its top files, discovering files and mutants.
fn walk_package(
    workspace_dir: &Utf8Path,
    package: &Package,
    progress: &WalkProgress,
    options: &Options,
) -> Result<(Vec<Mutant>, Vec<SourceFile>)> {
    let mut mutants = Vec::new();
    let mut files = Vec::new();
    // Files whose `mod` statements have not been followed yet. Handled one
    // breadth-first level at a time so that the files in a level can be walked
    // concurrently while preserving the discovery order a serial walk produces.
    let mut pending: Vec<(Utf8PathBuf, bool)> = package
        .top_sources
        .iter()
        .map(|p| (p.to_owned(), true))
        .collect();
    while !pending.is_empty() {
        check_interrupted()?;
        let level = std::mem::take(&mut pending);
        let walked = walk_files(workspace_dir, package, &level, options)?;
        for outcome in walked {
            let Some((source_file, mut file_mutants, external_mods)) = outcome else {
                // Resolved outside the tree; `walk_files` already logged it,
                // and it doesn't count towards either progress counter.
                continue;
            };
            progress.increment_files(1);
            progress.increment_mutants(file_mutants.len());
            // TODO: It would be better not to spend time generating mutants from
            // files that are not going to be visited later. However, we probably do
            // still want to walk them to find modules that are referenced by them.
            // since otherwise it could be pretty confusing that lower files are not
            // visited.
            //
            // We'll still walk down through files that don't match globs, so that
            // we have a chance to find modules underneath them. However, we won't
            // collect any mutants from them, and they don't count as "seen" for
            // `--list-files`.
            for mod_namespace in &external_mods {
                if let Some(mod_path) = find_mod_source(workspace_dir, &source_file, mod_namespace)
                {
                    pending.push((mod_path, false));
                }
            }
            if !options.allows_source_file_path(&source_file.tree_relative_path) {
                continue;
            }
            mutants.append(&mut file_mutants);
            files.push(source_file);
        }
    }
    Ok((mutants, files))
}

/// What walking one file yields: `None` if the path resolved outside the
/// tree, matching [`SourceFile::load`].
type WalkedFile = Option<(SourceFile, Vec<Mutant>, Vec<ExternalModRef>)>;

/// The per-file result of [`walk_files`], including a failure to load or
/// parse the file.
type FileOutcome = Result<WalkedFile>;

/// Load and walk a single pending file.
///
/// Loading (including CRLF normalization and line-indexing) and parsing both
/// happen here, on the file's own thread, so they overlap with every other
/// file's IO and parsing within the same level.
fn load_and_walk_file(
    workspace_dir: &Utf8Path,
    package: &Package,
    path: &Utf8Path,
    package_top: bool,
    options: &Options,
) -> FileOutcome {
    let Some(source_file) = SourceFile::load(workspace_dir, path, package, package_top)? else {
        info!("Skipping source file outside of tree: {path:?}");
        return Ok(None);
    };
    let (mutants, external_mods) = walk_file(&source_file, options)?;
    Ok(Some((source_file, mutants, external_mods)))
}

/// Walk a set of pending files, each loaded and parsed on its own thread,
/// returning results in input order.
///
/// Each file gets a fresh thread for two reasons. `proc_macro2`'s source map,
/// which backs every span lookup, is a thread-local that gains an entry per file
/// parsed and is scanned linearly on each lookup, so walking a whole tree on one
/// thread costs O(mutants x files); a thread per file keeps exactly one file in
/// the map. Running those threads concurrently then also uses the cores that
/// would otherwise sit idle throughout discovery.
///
/// At most `available_parallelism()` files are ever in flight: rather than
/// joining a whole fixed-size chunk before starting the next one (which lets
/// a single slow file stall every other core until the chunk completes), a
/// replacement file is spawned the moment any in-flight file finishes,
/// keeping the window full until the level runs out.
fn walk_files(
    workspace_dir: &Utf8Path,
    package: &Package,
    pending: &[(Utf8PathBuf, bool)],
    options: &Options,
) -> Result<Vec<WalkedFile>> {
    let concurrency = thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let n = pending.len();
    let mut results: Vec<Option<FileOutcome>> = std::iter::repeat_with(|| None).take(n).collect();
    let (tx, rx) = mpsc::channel::<(usize, thread::Result<FileOutcome>)>();
    thread::scope(|scope| {
        let spawn_at = |idx: usize| {
            let (path, package_top) = &pending[idx];
            let tx = tx.clone();
            scope.spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    load_and_walk_file(workspace_dir, package, path, *package_top, options)
                }));
                let _ = tx.send((idx, outcome));
            });
        };
        let mut next = 0;
        let mut in_flight = 0;
        while in_flight < concurrency && next < n {
            spawn_at(next);
            next += 1;
            in_flight += 1;
        }
        while in_flight > 0 {
            let (idx, outcome) = rx
                .recv()
                .expect("a walk_files worker exited without reporting a result");
            results[idx] = Some(outcome.unwrap_or_else(|payload| resume_unwind(payload)));
            in_flight -= 1;
            if next < n {
                spawn_at(next);
                next += 1;
                in_flight += 1;
            }
        }
    });
    results
        .into_iter()
        .map(|r| r.expect("every pending index should have produced a result"))
        .collect()
}

/// Find all possible mutants in a source file.
///
/// Returns the mutants found, and the names of modules referenced by `mod` statements
/// that should be visited later.
pub fn walk_file(
    source_file: &SourceFile,
    options: &Options,
) -> Result<(Vec<Mutant>, Vec<ExternalModRef>)> {
    let _span = debug_span!("source_file", path = source_file.tree_relative_slashes()).entered();
    trace!("visit source file");
    // Parsed here rather than passed in: `syn::Expr` is not `Send`, and this
    // function runs on a per-file thread.
    let error_exprs = options.parsed_error_exprs()?;
    let syn_file = syn::parse_str::<syn::File>(source_file.code())
        .with_context(|| format!("failed to parse {}", source_file.tree_relative_slashes()))?;
    let enums = EnumIndex::from_file(&syn_file);
    let mut visitor = DiscoveryVisitor {
        error_exprs: &error_exprs,
        enums: &enums,
        error: None,
        exclude_re_stack: Vec::new(),
        external_mods: Vec::new(),
        mutants: Vec::new(),
        mod_namespace_stack: Vec::new(),
        namespace_stack: Vec::new(),
        fn_stack: Vec::new(),
        fn_context_stack: Vec::new(),
        guard_spans: Vec::new(),
        closure_depth: 0,
        const_eval_depth: 0,
        source_file: source_file.clone(),
        options,
        control_flow_memo: ControlFlowMemo::default(),
    };
    visitor.visit_file(&syn_file);
    if let Some(err) = visitor.error {
        return Err(err);
    }
    Ok((visitor.mutants, visitor.external_mods))
}

/// For testing: parse and generate mutants from one single file provided as a string.
///
/// The source code is assumed to be named `src/main.rs` with a fixed package name.
#[cfg(test)]
pub fn mutate_source_str(code: &str, options: &Options) -> Result<Vec<Mutant>> {
    let source_file = SourceFile::for_tests(
        Utf8Path::new("src/main.rs"),
        code,
        "cargo-mutants-testdata-internal",
        true,
    );
    let (mutants, _) = walk_file(&source_file, options)?;
    Ok(mutants)
}

/// For testing: mutate some code, without needing an enclosing function.
///
/// Return the string representations of the mutants.
#[cfg(test)]
pub fn mutate_expr(code: &str) -> Vec<String> {
    let full_code = format!("fn test_harness() {{\n{code}\n}}\n");
    let options = Options::default();
    let source_file = SourceFile::for_tests(
        Utf8Path::new("src/main.rs"),
        &full_code,
        "cargo-mutants-testdata-internal",
        true,
    );
    match walk_file(&source_file, &options) {
        Ok((mutants, _)) => mutants
            .iter()
            .filter(|m| !m.name(false).ends_with("replace test_harness with ()"))
            .map(|m| m.name(false))
            .map(|n| {
                n.strip_prefix("src/main.rs: ")
                    .expect("expected to stripe file name")
                    .strip_suffix(" in test_harness")
                    .expect("expected function name at end of mutant name")
                    .to_owned()
            })
            .collect(),
        Err(err) => panic!("failed to mutate {code:?}: {err}"),
    }
}

/// Reference to an external module from a source file.
///
/// This is approximately a list of namespace components like `["foo", "bar"]` for
/// `foo::bar`, but each may also be decorated with a `#[path="..."]` attribute,
/// and they're attributed to a location in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalModRef {
    /// Namespace components of the module path
    parts: Vec<ModNamespace>,
}

/// Namespace for a module defined in a `mod foo { ... }` block or `mod foo;` statement
///
/// In the context of resolving modules, a module "path" (and to some extent "name") is ambiguous:
/// paths may describe a sequence of identifiers in code (e.g. `crate::foo::bar`) or sequence of
/// folder and file names on the filesystem (e.g. `src/foo/bar.rs`).
///
/// The field and method names in this struct distinguish between the uses of path elements.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModNamespace {
    /// Identifier of the module (e.g. `foo` for `mod foo;`)
    name: String,
    /// File location override for the module, if specified by `#[path="file"]`
    ///
    /// Note that `mod foo { ... }` blocks can also have a file location specified,
    /// which affects the filesystem location of all child `mod bar;` statements.
    path_attribute: Option<Utf8PathBuf>,
    /// Location of the module definition in the source file
    source_location: Span,
}

impl ModNamespace {
    /// Returns the name of the module for filesystem purposes
    fn get_filesystem_name(&self) -> &Utf8Path {
        self.path_attribute
            .as_ref()
            .map_or(Utf8Path::new(&self.name), Utf8PathBuf::as_path)
    }
}

/// `syn` visitor that recursively traverses the syntax tree, accumulating places
/// that could be mutated.
///
/// As it walks the tree, it accumulates within itself a list of mutation opportunities,
/// and other files referenced by `mod` statements that should be visited later.
struct DiscoveryVisitor<'o> {
    /// All the mutants generated by visiting the file.
    mutants: Vec<Mutant>,

    /// The file being visited.
    source_file: SourceFile,

    /// The stack of modules namespaces that we're currently inside, from
    /// visiting `mod foo { ... }` statements.
    ///
    /// This is a subsequence of `namespace_stack` (with `#[path="..."]` information),
    /// containing only elements that form a module path.
    mod_namespace_stack: Vec<ModNamespace>,

    /// The stack of namespaces, loosely defined, that we're inside.
    ///
    /// Basically these are names or strings that can be concatenated with `::`
    /// to form a name that meaningfully describes where we are; it might not
    /// exactly be valid Rust.
    ///
    /// For example, this includes mods, fns, impls, etc.
    namespace_stack: Vec<String>,

    /// The functions we're inside.
    ///
    /// Empty at the top level, often has one element, but potentially more if
    /// there are nested functions.
    fn_stack: Vec<Arc<Function>>,

    /// Per-function context for mutants that need to know something about the
    /// enclosing function, innermost last.
    fn_context_stack: Vec<FnContext>,

    /// Spans of the match arm guards we're inside, so that a guard that is a
    /// bool literal is not also mutated as a literal.
    guard_spans: Vec<Span>,

    /// How many closure or `async` bodies we're inside, whose return type is
    /// usually inferred and so unknown here.
    closure_depth: usize,

    /// How many nested compile-time-evaluated constructs we're inside: a
    /// `const`/`static` initializer, a `const fn` body, an array-length
    /// expression, or a const generic argument. A depth counter (rather than
    /// a boolean) so that leaving an inner one of these while still inside
    /// an outer one doesn't lose the outer's flag.
    const_eval_depth: usize,

    /// The names from `mod foo;` statements that should be visited later,
    /// namespaced relative to the source file
    external_mods: Vec<ExternalModRef>,

    /// Parsed error expressions, from the config file or command line.
    error_exprs: &'o [Expr],

    /// Unit variants of the enums declared in this file, for mutating values of
    /// those enum types.
    enums: &'o EnumIndex,

    options: &'o Options,

    /// Stack of compiled `RegexSet`s from `#[mutants::exclude_re("...")]` attributes.
    ///
    /// Only scopes that actually carry patterns are pushed, so this is usually
    /// empty. When collecting a mutant, all entries in the stack are checked.
    exclude_re_stack: Vec<RegexSet>,

    /// If set, an error occurred during visiting (e.g. invalid regex in an attribute).
    ///
    /// Since `Visit` trait methods cannot return errors, we store the error here
    /// and propagate it after the visitor finishes.
    error: Option<anyhow::Error>,

    /// Cache of `return`/`break`/`continue` containment, shared across the
    /// whole file so nested blocks reuse work already done by their
    /// enclosing block. See [`ControlFlowMemo`].
    control_flow_memo: ControlFlowMemo,
}

/// What the visitor needs to know about the function it's inside.
struct FnContext {
    /// Span of the function body excluding braces, if any.
    body_span: Option<Span>,
    /// Declared return type, for mutating `return` expressions.
    output: ReturnType,
    /// Whether the body is exactly one `return expr;` statement, in which case
    /// a `ReturnValue` mutant would duplicate the `FnValue` mutant.
    body_is_single_return: bool,
}

impl FnContext {
    fn new(sig: &Signature, block: &Block) -> FnContext {
        FnContext {
            body_span: function_body_span(block),
            output: sig.output.clone(),
            body_is_single_return: matches!(
                block.stmts.as_slice(),
                [syn::Stmt::Expr(Expr::Return(_), _)]
            ),
        }
    }
}

/// Whether entering a scope pushed an `exclude_re` entry that must be popped.
enum ExcludeScope {
    /// Patterns were compiled and pushed; the caller must pop them.
    Pushed,
    /// The scope has no `exclude_re` patterns, so nothing was pushed.
    Empty,
    /// An error was stored on the visitor; children must not be visited.
    Error,
}

impl DiscoveryVisitor<'_> {
    fn enter_function(
        &mut self,
        function_name: &Ident,
        return_type: &ReturnType,
        span: proc_macro2::Span,
    ) -> Arc<Function> {
        self.namespace_stack.push(function_name.to_string());
        let full_function_name = self.namespace_stack.join("::");
        let function = Arc::new(Function {
            function_name: full_function_name,
            return_type: return_type.to_pretty_string(),
            span: span.into(),
        });
        self.fn_stack.push(Arc::clone(&function));
        function
    }

    fn leave_function(&mut self, function: Arc<Function>) {
        self.namespace_stack
            .pop()
            .expect("Namespace stack should not be empty");
        assert_eq!(
            self.fn_stack.pop(),
            Some(function),
            "Function stack mismatch"
        );
    }

    /// Push `#[mutants::exclude_re("...")]` patterns from the given attributes onto the stack.
    ///
    /// Most items carry no `exclude_re` attribute, so the common result is
    /// [`ExcludeScope::Empty`]: nothing is pushed and nothing must be popped.
    /// Pushing an empty `RegexSet` instead would build a whole regex automaton
    /// per visited item, and leave dead entries for `excluded_by_attr_re` to
    /// scan for every mutant.
    ///
    /// Prefer [`Self::in_exclude_re_scope`] over calling push/pop directly so
    /// that early returns can't leave the stack unbalanced.
    fn push_exclude_re(&mut self, attrs: &[Attribute]) -> ExcludeScope {
        let patterns = match attrs_exclude_re_patterns(attrs) {
            Ok(patterns) => patterns,
            Err((span, msg)) => {
                let location = self
                    .source_file
                    .format_source_location(LineColumn::from(span.start()));
                self.error
                    .get_or_insert_with(|| anyhow!("{location}: {msg}"));
                return ExcludeScope::Error;
            }
        };
        if patterns.is_empty() {
            return ExcludeScope::Empty;
        }
        match RegexSet::new(&patterns) {
            Ok(re) => {
                self.exclude_re_stack.push(re);
                ExcludeScope::Pushed
            }
            Err(err) => {
                let location = attrs
                    .iter()
                    .find(|a| {
                        path_is(a.path(), &["mutants", "exclude_re"])
                            || path_is(a.path(), &["cfg_attr"])
                    })
                    .map_or_else(
                        || self.source_file.tree_relative_slashes().to_owned(),
                        |a| {
                            self.source_file
                                .format_source_location(LineColumn::from(a.span().start()))
                        },
                    );
                let quoted = patterns
                    .iter()
                    .map(|p| format!("\"{p}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.error.get_or_insert_with(|| {
                    anyhow!("{location}: invalid regex in #[mutants::exclude_re({quoted})]: {err}")
                });
                ExcludeScope::Error
            }
        }
    }

    /// Pop the most recent `exclude_re` scope.
    ///
    /// Callers should normally use [`Self::in_exclude_re_scope`] instead of
    /// calling [`Self::push_exclude_re`] and this directly, so that an early
    /// return inside the scope can't leave the stack unbalanced.
    fn pop_exclude_re(&mut self) {
        self.exclude_re_stack
            .pop()
            .expect("exclude_re stack should not be empty");
    }

    /// Run `f` inside a new `#[mutants::exclude_re("...")]` scope derived from `attrs`.
    ///
    /// On any kind of failure (invalid regex, malformed attribute), the error
    /// is stored on the visitor and `f` is not invoked. The scope is always
    /// popped after `f` returns, so callers don't need to worry about early
    /// returns inside `f` leaving the stack unbalanced.
    fn in_exclude_re_scope<F>(&mut self, attrs: &[Attribute], f: F)
    where
        F: FnOnce(&mut Self),
    {
        // `f` is called from exactly one place on purpose: it carries the whole
        // body of a `visit_*` method, so a second call site would duplicate
        // that code into every caller.
        let scope = self.push_exclude_re(attrs);
        if matches!(scope, ExcludeScope::Error) {
            return;
        }
        f(self);
        if matches!(scope, ExcludeScope::Pushed) {
            self.pop_exclude_re();
        }
    }

    /// Whether any currently-active `#[mutants::exclude_re("...")]` attribute
    /// rejects this mutant by name.
    ///
    /// The mutant's name is built only when some attribute is actually in
    /// scope, which is not the common case: with an empty stack the answer is
    /// `false` whatever the name is.
    fn excluded_by_attr_re(&self, mutant: &Mutant) -> bool {
        !self.exclude_re_stack.is_empty() && {
            let name = mutant.full_name();
            self.exclude_re_stack.iter().any(|re| re.is_match(name))
        }
    }

    /// Record that we generated some mutants.
    fn collect_mutant(
        &mut self,
        span: Span,
        short_replaced: Option<String>,
        replacement: &TokenStream,
        genre: Genre,
    ) {
        let mutant = Mutant::new_discovered(
            self.source_file.clone(),
            self.fn_stack.last().cloned(),
            span,
            short_replaced,
            replacement.to_pretty_string(),
            genre,
            None,
            self.const_eval_depth > 0,
        );
        if self.excluded_by_attr_re(&mutant) {
            trace!(
                name = mutant.name(false),
                "skip mutant by exclude_re attribute"
            );
        } else if self.options.allows_mutant(&mutant) {
            self.mutants.push(mutant);
        } else {
            trace!(name = mutant.name(false), "skip mutant by options");
        }
    }

    fn collect_fn_mutants(&mut self, sig: &Signature, block: &Block) {
        if let Some(function) = self.fn_stack.last().cloned() {
            let body_span = function_body_span(block).expect("Empty function body");
            let repls = return_type_replacements(&sig.output, self.error_exprs, self.enums);
            if repls.is_empty() {
                trace!(
                    function_name = function.function_name,
                    return_type = function.return_type,
                    "No mutants generated for this return type"
                );
            } else {
                let orig_block = block.to_token_stream().to_pretty_string();
                for rep in repls {
                    // Comparing strings is a kludge for proc_macro2 not (yet) apparently
                    // exposing any way to compare token streams...
                    //
                    // TODO: Maybe this should move into collect_mutant, but at the moment
                    // FnValue is the only genre that seems able to generate no-ops.
                    //
                    // The original block has braces and the replacements don't, so put
                    // them back for the comparison...
                    let new_block = quote!( { #rep } ).to_token_stream().to_pretty_string();
                    // dbg!(&orig_block, &new_block);
                    if orig_block == new_block {
                        trace!("Replacement is the same as the function body; skipping");
                    } else {
                        self.collect_mutant(body_span, None, &rep, Genre::FnValue);
                    }
                }
            }
        } else {
            warn!("collect_fn_mutants called while not in a function?");
        }
    }

    /// Whether this expression is a call that `--skip-calls` asks us to leave alone.
    ///
    /// [`Self::visit_expr_call`] and [`Self::visit_expr_method_call`] stop
    /// descending into such calls; a statement-level mutant is generated before
    /// that descent, so it has to make the same check.
    fn is_skipped_call(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Call(call) => match &*call.func {
                Expr::Path(ExprPath { path, .. }) => self
                    .options
                    .skip_calls
                    .iter()
                    .any(|s| path_ends_with(path, s)),
                _ => false,
            },
            Expr::MethodCall(method_call) => self
                .options
                .skip_calls
                .iter()
                .any(|s| method_call.method == s),
            _ => false,
        }
    }

    /// Call a function with a namespace pushed onto the stack.
    ///
    /// This is used when recursively descending into a namespace.
    fn in_namespace<F, T>(&mut self, name: &str, f: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        self.namespace_stack.push(name.to_owned());
        let r = f(self);
        assert_eq!(self.namespace_stack.pop().unwrap(), name);
        r
    }

    /// Run `f` with the compile-time-evaluated depth counter incremented.
    ///
    /// Used for constructs the compiler evaluates at compile time: `const`
    /// and `static` initializers, `const fn` bodies, array-length
    /// expressions, and const generic arguments. Mutants discovered while
    /// `f` runs are marked [`Mutant::const_eval`] so `--skip-uncovered`
    /// knows a line with no coverage counter there is not necessarily
    /// untestable.
    fn in_const_eval_scope<F, T>(&mut self, f: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        self.const_eval_depth += 1;
        let r = f(self);
        self.const_eval_depth -= 1;
        r
    }
}

impl<'ast> Visit<'ast> for DiscoveryVisitor<'_> {
    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        let _span = trace_span!("expr_call", line = i.span().start().line).entered();
        if attrs_excluded(&i.attrs) {
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            if let Expr::Path(ExprPath { path, .. }) = &*i.func {
                trace!(path = path.to_pretty_string(), "visit call");
                if let Some(hit) = v
                    .options
                    .skip_calls
                    .iter()
                    .find(|s| path_ends_with(path, s))
                {
                    trace!("skip call to {hit}");
                    return;
                }
            }
            syn::visit::visit_expr_call(v, i);
        });
    }

    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        let _span = trace_span!("expr_method_call", line = i.span().start().line).entered();
        if attrs_excluded(&i.attrs) {
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            if let Some(hit) = v.options.skip_calls.iter().find(|s| i.method == s) {
                trace!("skip method call to {hit}");
                return;
            }
            syn::visit::visit_expr_method_call(v, i);
        });
    }

    /// Visit a source file.
    fn visit_file(&mut self, i: &'ast File) {
        // No trace here; it's created per file for the whole visitor
        if attrs_excluded(&i.attrs) {
            trace!("file excluded by attrs");
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            syn::visit::visit_file(v, i);
        });
    }

    /// Visit top-level `fn foo()`.
    fn visit_item_fn(&mut self, i: &'ast ItemFn) {
        let function_name = i.sig.ident.to_pretty_string();
        let _span = trace_span!(
            "fn",
            line = i.sig.fn_token.span.start().line,
            name = function_name
        )
        .entered();
        trace!("visit fn");
        if fn_sig_excluded(&i.sig) || attrs_excluded(&i.attrs) || block_is_empty(&i.block) {
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            let function = v.enter_function(&i.sig.ident, &i.sig.output, i.span());
            let is_const_fn = i.sig.constness.is_some();
            let visit_body = |v: &mut Self| {
                v.collect_fn_mutants(&i.sig, &i.block);
                v.fn_context_stack.push(FnContext::new(&i.sig, &i.block));
                syn::visit::visit_item_fn(v, i);
                v.fn_context_stack.pop();
            };
            if is_const_fn {
                v.in_const_eval_scope(visit_body);
            } else {
                visit_body(v);
            }
            v.leave_function(function);
        });
    }

    /// Visit `fn foo()` within an `impl`.
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        // Don't look inside constructors (called "new") because there's often no good
        // alternative.
        let function_name = i.sig.ident.to_pretty_string();
        let _span = trace_span!(
            "fn",
            line = i.sig.fn_token.span.start().line,
            name = function_name
        )
        .entered();
        if fn_sig_excluded(&i.sig)
            || attrs_excluded(&i.attrs)
            || i.sig.ident == "new"
            || block_is_empty(&i.block)
        {
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            let function = v.enter_function(&i.sig.ident, &i.sig.output, i.span());
            let is_const_fn = i.sig.constness.is_some();
            let visit_body = |v: &mut Self| {
                v.collect_fn_mutants(&i.sig, &i.block);
                v.fn_context_stack.push(FnContext::new(&i.sig, &i.block));
                syn::visit::visit_impl_item_fn(v, i);
                v.fn_context_stack.pop();
            };
            if is_const_fn {
                v.in_const_eval_scope(visit_body);
            } else {
                visit_body(v);
            }
            v.leave_function(function);
        });
    }

    /// Visit `fn foo() { ... }` within a trait, i.e. a default implementation of a function.
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        let function_name = i.sig.ident.to_pretty_string();
        let _span = trace_span!(
            "fn",
            line = i.sig.fn_token.span.start().line,
            name = function_name
        )
        .entered();
        if fn_sig_excluded(&i.sig) || attrs_excluded(&i.attrs) || i.sig.ident == "new" {
            return;
        }
        if let Some(block) = &i.default {
            if block_is_empty(block) {
                return;
            }
            self.in_exclude_re_scope(&i.attrs, |v| {
                let function = v.enter_function(&i.sig.ident, &i.sig.output, i.span());
                let is_const_fn = i.sig.constness.is_some();
                let visit_body = |v: &mut Self| {
                    v.collect_fn_mutants(&i.sig, block);
                    v.fn_context_stack.push(FnContext::new(&i.sig, block));
                    syn::visit::visit_trait_item_fn(v, i);
                    v.fn_context_stack.pop();
                };
                if is_const_fn {
                    v.in_const_eval_scope(visit_body);
                } else {
                    visit_body(v);
                }
                v.leave_function(function);
            });
        }
    }

    /// Visit a top-level `const FOO: T = ...;` item.
    ///
    /// The visitor itself does not generate const-specific mutants, but it
    /// honours `#[mutants::skip]` on the item so that operator mutants inside
    /// the initializer expression can be suppressed.
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        let _span = trace_span!(
            "const",
            line = i.const_token.span.start().line,
            name = i.ident.to_pretty_string()
        )
        .entered();
        if attrs_excluded(&i.attrs) {
            trace!("const excluded by attrs");
            return;
        }
        self.in_const_eval_scope(|v| syn::visit::visit_item_const(v, i));
    }

    /// Visit a top-level `static FOO: T = ...;` item.
    ///
    /// As with [`Self::visit_item_const`], this only exists so that
    /// `#[mutants::skip]` on the item suppresses mutants inside the
    /// initializer expression.
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        let _span = trace_span!(
            "static",
            line = i.static_token.span.start().line,
            name = i.ident.to_pretty_string()
        )
        .entered();
        if attrs_excluded(&i.attrs) {
            trace!("static excluded by attrs");
            return;
        }
        self.in_const_eval_scope(|v| syn::visit::visit_item_static(v, i));
    }

    /// Visit an associated `const FOO: T = ...;` inside an `impl` block.
    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        let _span = trace_span!(
            "const",
            line = i.const_token.span.start().line,
            name = i.ident.to_pretty_string()
        )
        .entered();
        if attrs_excluded(&i.attrs) {
            trace!("associated const excluded by attrs");
            return;
        }
        self.in_const_eval_scope(|v| syn::visit::visit_impl_item_const(v, i));
    }

    /// Visit an associated `const FOO: T [= ...];` inside a `trait` block.
    ///
    /// Only the default-value expression (if present) contains anything that
    /// can be mutated; this method gates that descent on `#[mutants::skip]`.
    fn visit_trait_item_const(&mut self, i: &'ast syn::TraitItemConst) {
        let _span = trace_span!(
            "const",
            line = i.const_token.span.start().line,
            name = i.ident.to_pretty_string()
        )
        .entered();
        if attrs_excluded(&i.attrs) {
            trace!("trait associated const excluded by attrs");
            return;
        }
        self.in_const_eval_scope(|v| syn::visit::visit_trait_item_const(v, i));
    }

    /// Visit `[T; N]`: the length `N` is always evaluated at compile time,
    /// even where the array type itself appears in ordinary runtime code.
    fn visit_type_array(&mut self, i: &'ast syn::TypeArray) {
        self.in_const_eval_scope(|v| syn::visit::visit_type_array(v, i));
    }

    /// Visit one generic argument, e.g. the `N` in `Foo<N>`.
    ///
    /// Only [`syn::GenericArgument::Const`] is compile-time-evaluated; type
    /// and lifetime arguments are not expressions and the others carry their
    /// own nested `Expr`/`Type` that this default traversal still reaches.
    fn visit_generic_argument(&mut self, i: &'ast syn::GenericArgument) {
        if matches!(i, syn::GenericArgument::Const(_)) {
            self.in_const_eval_scope(|v| syn::visit::visit_generic_argument(v, i));
        } else {
            syn::visit::visit_generic_argument(self, i);
        }
    }

    /// Visit `impl Foo { ...}` or `impl Debug for Foo { ... }`.
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if attrs_excluded(&i.attrs) {
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            let type_name = i.self_ty.to_pretty_string();
            let name = if let Some((_, trait_path, _)) = &i.trait_ {
                if path_ends_with(trait_path, "Default") {
                    // Can't think of how to generate a viable different default.
                    return;
                }
                format!(
                    "<impl {trait} for {type_name}>",
                    trait = trait_path.to_pretty_string()
                )
            } else {
                type_name
            };
            v.in_namespace(&name, |vv| syn::visit::visit_item_impl(vv, i));
        });
    }

    /// Visit `trait Foo { ... }`
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        let name = i.ident.to_pretty_string();
        let _span = trace_span!("trait", line = i.span().start().line, name).entered();
        if attrs_excluded(&i.attrs) {
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            v.in_namespace(&name, |vv| syn::visit::visit_item_trait(vv, i));
        });
    }

    /// Visit `mod foo { ... }` or `mod foo;`.
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let mod_name = node.ident.unraw().to_string();
        let _span = trace_span!("mod", line = node.mod_token.span.start().line, mod_name).entered();
        if attrs_excluded(&node.attrs) {
            trace!("mod excluded by attrs");
            return;
        }
        self.in_exclude_re_scope(&node.attrs, |v| {
            let source_location = Span::from(node.span());

            // Extract path attribute value, if any (e.g. `#[path="..."]`)
            let path_attribute = match find_path_attribute(&node.attrs) {
                Ok(path) => path,
                Err(path_attribute) => {
                    let definition_site = v
                        .source_file
                        .format_source_location(source_location.start);
                    error!(?path_attribute, ?definition_site, %mod_name, "invalid filesystem traversal in mod path attribute");
                    return;
                }
            };
            let mod_namespace = ModNamespace {
                name: mod_name.clone(),
                path_attribute,
                source_location,
            };
            v.mod_namespace_stack.push(mod_namespace.clone());

            // If there's no content in braces, then this is a `mod foo;`
            // statement referring to an external file. We remember the module
            // name and then later look for the file.
            if node.content.is_none() {
                // If we're already inside `mod a { ... }` and see `mod b;` then
                // remember [a, b] as an external module to visit later.
                v.external_mods.push(ExternalModRef {
                    parts: v.mod_namespace_stack.clone(),
                });
            }
            v.in_namespace(&mod_namespace.name, |vv| {
                syn::visit::visit_item_mod(vv, node);
            });
            assert_eq!(v.mod_namespace_stack.pop(), Some(mod_namespace));
        });
    }

    /// Visit `a op b` expressions.
    fn visit_expr_binary(&mut self, i: &'ast syn::ExprBinary) {
        let _span = trace_span!("binary", line = i.op.span().start().line).entered();
        trace!("visit binary operator");
        if attrs_excluded(&i.attrs) {
            return;
        }
        // Note: `#[mutants::exclude_re("...")]` placed before a binary expression
        // is absorbed by syn into the leftmost operand's attrs (e.g., the path
        // expression), not into `ExprBinary.attrs`. There's therefore no current
        // Rust syntax that would populate `i.attrs` here, so we don't push an
        // exclude_re scope. To suppress mutants on a binary operator, place the
        // attribute on an enclosing item, mod, file, or expression
        // (call/method_call/match/struct).
        let replacements = match i.op {
            // We don't generate `<=` from `==` because it can too easily go
            // wrong with unsigned types compared to 0.

            // We try replacing logical ops with == and !=, which are effectively
            // XNOR and XOR when applied to booleans. However, they're often unviable
            // because they require parenthesis for disambiguation in many expressions.
            BinOp::Eq(_) => vec![quote! { != }],
            BinOp::Ne(_) => vec![quote! { == }],
            BinOp::And(_) => vec![quote! { || }],
            BinOp::Or(_) => vec![quote! { && }],
            BinOp::Lt(_) => vec![quote! { == }, quote! {>}, quote! { <= }],
            BinOp::Gt(_) => vec![quote! { == }, quote! {<}, quote! { >= }],
            BinOp::Le(_) => vec![quote! {>}, quote! {<}],
            BinOp::Ge(_) => vec![quote! {<}, quote! {>}],
            BinOp::Add(_) => vec![quote! {-}, quote! {*}],
            BinOp::AddAssign(_) => vec![quote! {-=}, quote! {*=}],
            BinOp::Sub(_) | BinOp::Mul(_) => vec![quote! {+}, quote! {/}],
            BinOp::SubAssign(_) | BinOp::MulAssign(_) => vec![quote! {+=}, quote! {/=}],
            BinOp::Div(_) => vec![quote! {%}, quote! {*}],
            BinOp::DivAssign(_) => vec![quote! {%=}, quote! {*=}],
            BinOp::Rem(_) => vec![quote! {/}, quote! {+}],
            BinOp::RemAssign(_) => vec![quote! {/=}, quote! {+=}],
            BinOp::Shl(_) => vec![quote! {>>}],
            BinOp::ShlAssign(_) => vec![quote! {>>=}],
            BinOp::Shr(_) => vec![quote! {<<}],
            BinOp::ShrAssign(_) => vec![quote! {<<=}],
            BinOp::BitAnd(_) => vec![quote! {|}, quote! {^}],
            BinOp::BitAndAssign(_) => vec![quote! {|=}],
            BinOp::BitOr(_) => vec![quote! {&}, quote! {^}],
            BinOp::BitOrAssign(_) => vec![quote! {&=}],
            BinOp::BitXor(_) => vec![quote! {|}, quote! {&}],
            BinOp::BitXorAssign(_) => vec![quote! {|=}, quote! {&=}],
            _ => {
                trace!(
                    op = i.op.to_pretty_string(),
                    "No mutants generated for this binary operator"
                );
                Vec::new()
            }
        };
        for rep in replacements {
            self.collect_mutant(i.op.span().into(), None, &rep, Genre::BinaryOperator);
        }
        syn::visit::visit_expr_binary(self, i);
    }

    fn visit_expr_unary(&mut self, i: &'ast syn::ExprUnary) {
        let _span = trace_span!("unary", line = i.op.span().start().line).entered();
        trace!("visit unary operator");
        if attrs_excluded(&i.attrs) {
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            match i.op {
                UnOp::Not(_) | UnOp::Neg(_) => {
                    v.collect_mutant(i.op.span().into(), None, &quote! {}, Genre::UnaryOperator);
                }
                _ => {
                    trace!(
                        op = i.op.to_pretty_string(),
                        "No mutants generated for this unary operator"
                    );
                }
            }
            syn::visit::visit_expr_unary(v, i);
        });
    }

    fn visit_expr_if(&mut self, i: &'ast syn::ExprIf) {
        let _span = trace_span!("if", line = i.if_token.span.start().line).entered();
        if attrs_excluded(&i.attrs) {
            trace!("if excluded by attrs");
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            // `if let` conditions are not `bool` and can't be replaced by a literal.
            // A condition that already *is* a bool literal is covered by `BoolLiteral`.
            if !matches!(&*i.cond, Expr::Let(_) | Expr::Lit(_)) {
                let span: Span = i.cond.span().into();
                v.collect_mutant(span, None, &quote! { true }, Genre::IfCondition);
                // `if cond { break }` is how a `loop` ends: replacing the
                // condition with `false` removes the only exit and hangs, which
                // burns the whole per-mutant timeout without telling us anything
                // about the tests. `true` is still generated: it makes the loop
                // end early.
                if v.control_flow_memo.block_contains_loop_exit(&i.then_branch) {
                    trace!("skip `false` for an if that exits a loop");
                } else {
                    v.collect_mutant(span, None, &quote! { false }, Genre::IfCondition);
                }
            }
            syn::visit::visit_expr_if(v, i);
        });
    }

    fn visit_expr_while(&mut self, i: &'ast syn::ExprWhile) {
        let _span = trace_span!("while", line = i.while_token.span.start().line).entered();
        if attrs_excluded(&i.attrs) {
            trace!("while excluded by attrs");
            return;
        }
        self.in_exclude_re_scope(&i.attrs, |v| {
            // Only `false`: `while true` hangs any loop without an unconditional
            // `break`, burning the whole per-mutant timeout without distinguishing
            // a good test suite from a bad one.
            if !matches!(&*i.cond, Expr::Let(_) | Expr::Lit(_)) {
                v.collect_mutant(
                    i.cond.span().into(),
                    None,
                    &quote! { false },
                    Genre::WhileCondition,
                );
            }
            syn::visit::visit_expr_while(v, i);
        });
    }

    fn visit_expr_match(&mut self, i: &'ast syn::ExprMatch) {
        let _span = trace_span!("match", line = i.span().start().line).entered();

        // While it's not currently possible to annotate expressions with custom attributes, this
        // limitation could be lifted in the future.
        if attrs_excluded(&i.attrs) {
            trace!("match excluded by attrs");
            return;
        }

        self.in_exclude_re_scope(&i.attrs, |v| {
            let has_catchall = i
                .arms
                .iter()
                .any(|arm| matches!(arm.pat, syn::Pat::Wild(_)));
            if has_catchall {
                // Successively delete each of the match arms, allowing them to hit the catch-all arm,
                // which will often be a viable mutant.
                for arm in &i.arms {
                    if matches!(arm.pat, syn::Pat::Wild(_)) || arm.guard.is_some() {
                        // Don't delete the `_ => whatever` arm, because that will very likely fail to compile.
                        //
                        // Also, don't delete arms with guard expressions, because the replacement of
                        // the guard with 'false' below is logically equivalent to removing the arm.
                        continue;
                    }
                    let replacement = quote! {};
                    v.collect_mutant(
                        arm.span().into(),
                        Some(arm.pat.to_pretty_string()),
                        &replacement,
                        Genre::MatchArm,
                    );
                }
            } else {
                trace!("match has no `_` pattern");
            }

            i.arms
                .iter()
                .flat_map(|arm| &arm.guard)
                .for_each(|(_if, guard_expr)| {
                    v.collect_mutant(
                        guard_expr.span().into(),
                        None,
                        &quote! { true },
                        Genre::MatchArmGuard,
                    );
                    v.collect_mutant(
                        guard_expr.span().into(),
                        None,
                        &quote! { false },
                        Genre::MatchArmGuard,
                    );
                });

            let guards_below = v.guard_spans.len();
            v.guard_spans.extend(
                i.arms
                    .iter()
                    .flat_map(|arm| &arm.guard)
                    .map(|(_if, guard_expr)| -> Span { guard_expr.span().into() }),
            );
            syn::visit::visit_expr_match(v, i);
            v.guard_spans.truncate(guards_below);
        });
    }

    /// Never mutate anything inside an attribute.
    ///
    /// `syn`'s default traversal descends from the attributes of every item into
    /// `Meta::NameValue` and then into an `Expr`, so without this a literal in
    /// `#[doc = "..."]` or `#[foo = true]` would be treated as mutable code.
    /// Attributes that affect mutation are read explicitly (`attrs_excluded`,
    /// `in_exclude_re_scope`), never through this traversal.
    fn visit_attribute(&mut self, _i: &'ast Attribute) {}

    /// Never mutate anything inside a pattern.
    ///
    /// `syn` routes `Pat::Lit` through `visit_expr_lit`, and replacing `true` with
    /// `false` in `match b { true => .., false => .. }` makes the match
    /// non-exhaustive (E0004). Patterns contain no other mutable expressions.
    fn visit_pat(&mut self, _i: &'ast syn::Pat) {}

    /// Visit a literal expression, mutating `true` and `false` in place.
    fn visit_expr_lit(&mut self, i: &'ast syn::ExprLit) {
        if let syn::Lit::Bool(lit_bool) = &i.lit {
            let span: Span = i.span().into();
            // A literal that is the whole function body, or a whole match arm
            // guard, is already mutated by `FnValue` or `MatchArmGuard`.
            let duplicates_fn_value = self
                .fn_context_stack
                .last()
                .is_some_and(|c| c.body_span == Some(span));
            let duplicates_match_guard = self.guard_spans.contains(&span);
            if !duplicates_fn_value && !duplicates_match_guard {
                let replacement = if lit_bool.value {
                    quote! { false }
                } else {
                    quote! { true }
                };
                self.collect_mutant(span, None, &replacement, Genre::BoolLiteral);
            }
        }
        syn::visit::visit_expr_lit(self, i);
    }

    fn visit_expr_struct(&mut self, i: &'ast syn::ExprStruct) {
        let _span = trace_span!("struct", line = i.span().start().line).entered();
        trace!("visit struct expression");

        if attrs_excluded(&i.attrs) {
            return;
        }

        self.in_exclude_re_scope(&i.attrs, |v| {
            // Check if this struct has a base (default) expression like `..Default::default()`
            if let Some(_rest) = &i.rest {
                // Get the struct type name
                let struct_name = i.path.to_pretty_string();

                // Generate a mutant for each field by deleting it
                // We need to include the trailing comma in the span
                for pair in i.fields.pairs() {
                    let field = pair.value();
                    if let syn::Member::Named(field_name) = &field.member {
                        let field_name_str = field_name.to_string();
                        // Span includes the field and its trailing comma (if present)
                        let span = if let Some(comma) = pair.punct() {
                            // Include the comma in the span
                            let field_span = field.span();
                            let comma_span = comma.span();
                            Span {
                                start: field_span.start().into(),
                                end: comma_span.end().into(),
                            }
                        } else {
                            // No comma, just the field
                            field.span().into()
                        };
                        let mutant = Mutant::new_discovered(
                            v.source_file.clone(),
                            v.fn_stack.last().cloned(),
                            span,
                            None,
                            String::new(),
                            Genre::StructField,
                            Some(MutationTarget::StructLiteralField {
                                field_name: field_name_str,
                                struct_name: struct_name.clone(),
                            }),
                            v.const_eval_depth > 0,
                        );
                        if !v.excluded_by_attr_re(&mutant) {
                            v.mutants.push(mutant);
                        }
                    }
                }
            }

            syn::visit::visit_expr_struct(v, i);
        });
    }

    /// Visit a block, generating a mutant that deletes each discardable statement.
    fn visit_block(&mut self, i: &'ast Block) {
        // Locals declared in this block without a type annotation: deleting a
        // statement that is the only use of one of them can make its type
        // uninferable (E0282, as in `let mut v = Vec::new(); v.push(1);`) or leave
        // it uninitialized (E0381).
        let untyped_locals = untyped_local_idents(i);
        for stmt in &i.stmts {
            if let Stmt::Expr(expr, Some(semi)) = stmt
                && is_deletable_statement(expr, &untyped_locals, &mut self.control_flow_memo)
                && !self.is_skipped_call(expr)
            {
                let span = Span {
                    start: expr.span().start().into(),
                    end: semi.span().end().into(),
                };
                self.collect_mutant(span, None, &quote! {}, Genre::DeleteStatement);
            }
        }
        syn::visit::visit_block(self, i);
    }

    fn visit_expr_closure(&mut self, i: &'ast syn::ExprClosure) {
        // A `return` inside a closure returns from the closure, whose return type
        // is usually inferred and so unknown here.
        self.closure_depth += 1;
        syn::visit::visit_expr_closure(self, i);
        self.closure_depth -= 1;
    }

    fn visit_expr_async(&mut self, i: &'ast syn::ExprAsync) {
        self.closure_depth += 1;
        syn::visit::visit_expr_async(self, i);
        self.closure_depth -= 1;
    }

    /// Visit `return expr`, replacing the returned value with fixed values of
    /// the enclosing function's declared return type.
    fn visit_expr_return(&mut self, i: &'ast syn::ExprReturn) {
        if self.closure_depth == 0
            && let Some(expr) = &i.expr
            && let Some(context) = self.fn_context_stack.last()
            // A function whose whole body is `return expr;` already has an
            // identical `FnValue` mutant.
            && !context.body_is_single_return
            && let ReturnType::Type(..) = &context.output
        {
            let original = expr.to_token_stream().to_pretty_string();
            let span: Span = expr.span().into();
            let output = context.output.clone();
            for rep in return_type_replacements(&output, self.error_exprs, self.enums) {
                if rep.to_pretty_string() == original {
                    trace!("Replacement is the same as the returned expression; skipping");
                } else {
                    self.collect_mutant(span, None, &rep, Genre::ReturnValue);
                }
            }
        }
        syn::visit::visit_expr_return(self, i);
    }
}

// Get the span of the block excluding the braces, or None if it is empty.
fn function_body_span(block: &Block) -> Option<Span> {
    Some(Span {
        start: block.stmts.first()?.span().start().into(),
        end: block.stmts.last()?.span().end().into(),
    })
}

/// Idents bound by `let` in this block with no type annotation.
fn untyped_local_idents(block: &Block) -> HashSet<String> {
    block
        .stmts
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::Local(local) => match &local.pat {
                syn::Pat::Ident(pat_ident) => Some(pat_ident.ident.to_string()),
                // `let x: T = ...` is `Pat::Type`, whose type is known without
                // looking at any later statement.
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Whether a statement expression can be deleted without changing the types of
/// anything around it.
///
/// Accepts only calls, method calls, and plain assignments: a statement with a
/// semicolon has type `()` regardless of the expression's type, so removing it
/// can't change the block's type. Compound assignments (`n -= 1`) are
/// `Expr::Binary` in syn and so are excluded, which also keeps us from deleting
/// the increment of a loop and hanging.
fn is_deletable_statement(
    expr: &Expr,
    untyped_locals: &HashSet<String>,
    control_flow_memo: &mut ControlFlowMemo,
) -> bool {
    let root = match expr {
        Expr::MethodCall(method_call) => root_ident(&method_call.receiver),
        Expr::Assign(assign) => root_ident(&assign.left),
        Expr::Call(_) => None,
        _ => return false,
    };
    if root.is_some_and(|ident| untyped_locals.contains(&ident)) {
        return false;
    }
    !control_flow_memo.contains_control_flow(expr)
}

/// The base ident of a place expression: `v` for `v`, `v.field`, `v[0]`, `*v`.
fn root_ident(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(ExprPath { path, .. }) => path.get_ident().map(ToString::to_string),
        Expr::Field(field) => root_ident(&field.base),
        Expr::Index(index) => root_ident(&index.expr),
        Expr::Unary(unary) if matches!(unary.op, UnOp::Deref(_)) => root_ident(&unary.expr),
        Expr::Paren(paren) => root_ident(&paren.expr),
        _ => None,
    }
}

/// Whether `return`, `break`, or `continue` occurs within a syntax subtree,
/// memoized by node identity.
///
/// `is_deletable_statement` and `visit_expr_if` ask this question of every
/// statement/`if` in a file, and `syn::visit::visit_block` then descends
/// into nested blocks and asks it again of their statements. Without
/// memoization, a chain of D nested single-statement blocks costs O(D^2):
/// each of the D queries walks the whole remaining subtree. Caching each
/// node's answer the first time it is fully walked makes the total work
/// linear in the size of the AST, because the first (outermost) query
/// populates the cache for every node underneath it, and every later query
/// on a node it already covered is an O(1) lookup.
///
/// Keyed on the raw pointer of the node: the AST is borrowed for the whole
/// file walk and never mutated, so pointers stay valid and stable for the
/// lifetime of this cache.
///
/// This intentionally mirrors the exact traversal of the previous
/// non-memoized `ControlFlowFinder`: only `return`, `break`, and `continue`
/// expressions are treated as leaves (their own sub-expressions, e.g. the
/// value of `return expr`, are not walked), and every other node — including
/// nested closures and loops — is walked as `syn::visit`'s default
/// implementation would.

#[derive(Default)]
struct ControlFlowMemo {
    /// Whether a `return` has been seen in the subtree currently being walked.
    returns: bool,
    /// Whether a `break` or `continue` has been seen in the subtree currently being walked.
    loop_exits: bool,
    expr_memo: HashMap<*const Expr, (bool, bool)>,
    block_memo: HashMap<*const Block, (bool, bool)>,
}

impl ControlFlowMemo {
    /// Whether the expression contains `return`, `break`, or `continue` anywhere.
    fn contains_control_flow(&mut self, expr: &Expr) -> bool {
        let (returns, loop_exits) = self.query_expr(expr);
        returns || loop_exits
    }

    /// Whether the block contains a `break` or `continue` anywhere, i.e. whether
    /// reaching it is how some enclosing loop ends or advances.
    fn block_contains_loop_exit(&mut self, block: &Block) -> bool {
        self.query_block(block).1
    }

    /// Compute (or fetch the cached) `(returns, loop_exits)` for `expr`, and
    /// fold that answer into the accumulator for whichever subtree is
    /// currently being walked.
    fn query_expr(&mut self, expr: &Expr) -> (bool, bool) {
        let ptr: *const Expr = expr;
        let mine = if let Some(&cached) = self.expr_memo.get(&ptr) {
            cached
        } else {
            let outer = (self.returns, self.loop_exits);
            self.returns = false;
            self.loop_exits = false;
            syn::visit::visit_expr(self, expr);
            let mine = (self.returns, self.loop_exits);
            self.expr_memo.insert(ptr, mine);
            self.returns = outer.0;
            self.loop_exits = outer.1;
            mine
        };
        self.returns |= mine.0;
        self.loop_exits |= mine.1;
        mine
    }

    /// As [`Self::query_expr`], but for a block.
    fn query_block(&mut self, block: &Block) -> (bool, bool) {
        let ptr: *const Block = block;
        let mine = if let Some(&cached) = self.block_memo.get(&ptr) {
            cached
        } else {
            let outer = (self.returns, self.loop_exits);
            self.returns = false;
            self.loop_exits = false;
            syn::visit::visit_block(self, block);
            let mine = (self.returns, self.loop_exits);
            self.block_memo.insert(ptr, mine);
            self.returns = outer.0;
            self.loop_exits = outer.1;
            mine
        };
        self.returns |= mine.0;
        self.loop_exits |= mine.1;
        mine
    }
}

impl<'ast> Visit<'ast> for ControlFlowMemo {
    fn visit_expr(&mut self, i: &'ast Expr) {
        self.query_expr(i);
    }
    fn visit_block(&mut self, i: &'ast Block) {
        self.query_block(i);
    }
    // `return`/`break`/`continue` are leaves for this analysis: their own
    // sub-expressions are deliberately not walked, matching the previous
    // `ControlFlowFinder`.
    fn visit_expr_return(&mut self, _i: &'ast syn::ExprReturn) {
        self.returns = true;
    }
    fn visit_expr_break(&mut self, _i: &'ast syn::ExprBreak) {
        self.loop_exits = true;
    }
    fn visit_expr_continue(&mut self, _i: &'ast syn::ExprContinue) {
        self.loop_exits = true;
    }
}

/// Find a new source file referenced by a `mod` statement.
///
/// Possibly, our heuristics just won't be able to find which file it is,
/// in which case we return `Ok(None)`.
fn find_mod_source(
    tree_root: &Utf8Path,
    parent: &SourceFile,
    mod_namespace: &ExternalModRef,
) -> Option<Utf8PathBuf> {
    // First, work out whether the mod will be a sibling in the same directory, or
    // in a child directory.
    //
    // 1. The parent is "src/foo.rs" and `mod bar` means "src/foo/bar.rs".
    //
    // 2. The parent is "src/lib.rs" (a target top file) and `mod bar` means "src/bar.rs".
    //
    // 3. The parent is "src/foo/mod.rs" and so `mod bar` means "src/foo/bar.rs".
    //
    // 4. A path attribute on a mod statement when there is no enclosing mod block
    //     E.g. for parent file "src/a/parent_file.rs",
    //     ```
    //     // `path` is relative to the directory where the source file is located
    //     #[path="foo_file.rs"] // resolves to: src/a/foo_file.rs
    //     mod foo;
    //
    //     mod bar {
    //         // `path` is relative to the directory of the enclosing module block
    //         #[path="baz_file.rs"] // resolves to: src/a/parent_file/bar/baz_file.rs
    //         mod baz;
    //     }
    //     ```
    //
    // Having determined the right directory then we can follow the path attribute, or
    // if no path is specified, then look for either `foo.rs` or `foo/mod.rs`.

    let (mod_child, mod_parents) = mod_namespace
        .parts
        .split_last()
        .expect("mod namespace is empty");

    // TODO: Beyond #115, we should probably remove all special handling of
    // `mod.rs` here by remembering how we found this file, and whether it
    // is above or inside the directory corresponding to its module?

    let parent_path = &parent.tree_relative_path;
    let mut search_dir = if parent.is_top
        || parent_path.ends_with("mod.rs")
        // NOTE: Path attribute on a top-level `mod foo;` (no enclosing block)
        //       ignores the parent module path
        || (mod_child.path_attribute.is_some() && mod_parents.is_empty())
    {
        parent_path
            .parent()
            .expect("mod path has no parent")
            .to_owned() // src/lib.rs -> src/
    } else {
        parent_path.with_extension("") // foo.rs -> foo/
    };

    search_dir.extend(mod_parents.iter().map(ModNamespace::get_filesystem_name));

    let mod_child_candidates = if let Some(filesystem_name) = &mod_child.path_attribute {
        vec![search_dir.join(filesystem_name)]
    } else {
        [".rs", "/mod.rs"]
            .iter()
            .map(|tail| search_dir.join(mod_child.name.clone() + tail))
            .collect()
    };

    let mut tried_paths = Vec::new();
    for relative_path in mod_child_candidates {
        let full_path = tree_root.join(&relative_path);
        if full_path.is_file() {
            trace!("found submodule in {full_path}");
            return Some(relative_path);
        }
        tried_paths.push(full_path);
    }
    let mod_name = &mod_child.name;
    let definition_site = parent.format_source_location(mod_child.source_location.start);
    warn!(?definition_site, %mod_name, ?tried_paths, "referent of mod not found");
    None
}

/// True if the signature of a function is such that it should be excluded.
fn fn_sig_excluded(sig: &syn::Signature) -> bool {
    if sig.unsafety.is_some() {
        trace!("Skip unsafe fn");
        true
    } else {
        false
    }
}

/// True if any of the attrs indicate that we should skip this node and everything inside it.
///
/// This checks for `#[cfg(test)]`, attributes whose path ends with `test` (like `#[test]` or `#[tokio::test]`), and `#[mutants::skip]`.
fn attrs_excluded(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr_is_cfg_test(attr) || attr_is_test(attr) || attr_is_mutants_skip(attr))
}

/// True if the block (e.g. the contents of a function) is empty.
fn block_is_empty(block: &syn::Block) -> bool {
    block.stmts.is_empty()
}

/// True if the attribute looks like `#[cfg(test)]`, or has "test"
/// anywhere in it.
fn attr_is_cfg_test(attr: &Attribute) -> bool {
    if !path_is(attr.path(), &["cfg"]) {
        return false;
    }
    let mut contains_test = false;
    if let Err(err) = attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("test") {
            contains_test = true;
        }
        Ok(())
    }) {
        trace!(
            ?err,
            attr = attr.to_pretty_string(),
            "Attribute is in an unrecognized form so skipped",
        );
        return false;
    }
    contains_test
}

/// True if the attribute path ends with `test`.
///
/// This matches `#[test]`, `#[tokio::test]`, `#[sqlx::test]`, etc.
fn attr_is_test(attr: &Attribute) -> bool {
    path_ends_with(attr.path(), "test")
}

fn path_is(path: &syn::Path, idents: &[&str]) -> bool {
    path.segments.iter().map(|ps| &ps.ident).eq(idents.iter())
}

/// True if the path ends with this identifier.
///
/// This is used as a heuristic to match types without being sensitive to which
/// module they are in, or to match functions without being sensitive to which
/// type they might be associated with.
///
/// This does not check type arguments.
fn path_ends_with(path: &syn::Path, ident: &str) -> bool {
    path.segments.last().is_some_and(|s| s.ident == ident)
}

/// True if the attribute contains `mutants::skip`.
///
/// This for example returns true for `#[mutants::skip]` or `#[cfg_attr(test, mutants::skip)]`.
fn attr_is_mutants_skip(attr: &Attribute) -> bool {
    if path_is(attr.path(), &["mutants", "skip"]) {
        return true;
    }
    if !path_is(attr.path(), &["cfg_attr"]) {
        return false;
    }
    let mut skip = false;
    if let Err(err) = attr.parse_nested_meta(|meta| {
        if path_is(&meta.path, &["mutants", "skip"]) {
            skip = true;
        }
        Ok(())
    }) {
        trace!(
            ?attr,
            ?err,
            "Attribute is not a path with attributes; skipping"
        );
        return false;
    }
    skip
}

/// Extract regex patterns from `#[mutants::exclude_re("...")]` attributes.
///
/// This also handles `#[cfg_attr(test, mutants::exclude_re("..."))]`, where
/// the cfg condition is ignored (we always look for the attribute, matching
/// the existing handling of `mutants::skip` within `cfg_attr`).
///
/// Returns the patterns in order if every relevant attribute parsed cleanly,
/// or a `(span, message)` describing the first malformed occurrence. The
/// span identifies the offending attribute so the caller can report a
/// source location to the user.
fn attrs_exclude_re_patterns(
    attrs: &[Attribute],
) -> std::result::Result<Vec<String>, (proc_macro2::Span, String)> {
    let mut patterns = Vec::new();
    for attr in attrs {
        if path_is(attr.path(), &["mutants", "exclude_re"]) {
            let pattern = parse_exclude_re_args(attr).map_err(|err| {
                (
                    attr.span(),
                    format!("malformed #[mutants::exclude_re(...)] attribute: {err}"),
                )
            })?;
            patterns.push(pattern);
        } else if path_is(attr.path(), &["cfg_attr"]) {
            collect_exclude_re_from_cfg_attr(attr, &mut patterns)?;
        }
    }
    Ok(patterns)
}

/// Parse the arguments of a direct `#[mutants::exclude_re("pat")]` attribute.
///
/// Requires exactly one string literal argument; everything else is a parse
/// error.
fn parse_exclude_re_args(attr: &Attribute) -> syn::Result<String> {
    attr.parse_args_with(|input: syn::parse::ParseStream| -> syn::Result<String> {
        let lit: syn::LitStr = input.parse()?;
        if !input.is_empty() {
            return Err(input
                .error("expected exactly one string literal argument to #[mutants::exclude_re]"));
        }
        Ok(lit.value())
    })
}

/// Walk a `#[cfg_attr(..., mutants::exclude_re("pat"), ...)]` attribute,
/// appending every found pattern to `patterns`.
///
/// If the `cfg_attr` itself cannot be parsed as nested meta (e.g. it has a
/// cfg condition shape we don't understand here), the attribute is
/// silently ignored — matching how [`attr_is_mutants_skip`] handles the
/// same case. But if we successfully locate a `mutants::exclude_re` inside
/// and its arguments are malformed, that is reported as a hard error so
/// users don't think their exclude is in effect when it isn't.
fn collect_exclude_re_from_cfg_attr(
    attr: &Attribute,
    patterns: &mut Vec<String>,
) -> std::result::Result<(), (proc_macro2::Span, String)> {
    let mut malformed: Option<(proc_macro2::Span, String)> = None;
    let parse_result = attr.parse_nested_meta(|meta| {
        if !path_is(&meta.path, &["mutants", "exclude_re"]) {
            return Ok(());
        }
        let span = meta.path.span();
        let inner: syn::Result<String> = (|| {
            let content;
            syn::parenthesized!(content in meta.input);
            let lit: syn::LitStr = content.parse()?;
            if !content.is_empty() {
                return Err(content.error(
                    "expected exactly one string literal argument to #[mutants::exclude_re]",
                ));
            }
            Ok(lit.value())
        })();
        match inner {
            Ok(pattern) => {
                patterns.push(pattern);
                Ok(())
            }
            Err(err) => {
                malformed.get_or_insert((
                    span,
                    format!("malformed #[mutants::exclude_re(...)] inside cfg_attr: {err}"),
                ));
                // Propagate to abort parse_nested_meta; the recorded error
                // takes precedence over the parse_result.
                Err(err)
            }
        }
    });
    if let Some(err) = malformed {
        return Err(err);
    }
    if let Err(err) = parse_result {
        trace!(?attr, ?err, "Could not fully parse cfg_attr nested meta");
    }
    Ok(())
}

/// Finds the first path attribute (`#[path = "..."]`)
///
/// # Errors
/// Returns an error if the path attribute contains a dubious path (leading `/`)
fn find_path_attribute(attrs: &[Attribute]) -> std::result::Result<Option<Utf8PathBuf>, String> {
    attrs
        .iter()
        .find_map(|attr| match &attr.meta {
            syn::Meta::NameValue(meta) if meta.path.is_ident("path") => {
                let syn::Expr::Lit(expr_lit) = &meta.value else {
                    return None;
                };
                let syn::Lit::Str(lit_str) = &expr_lit.lit else {
                    return None;
                };
                let path = lit_str.value();

                // refuse to follow absolute paths
                if path.starts_with('/') {
                    Some(Err(path))
                } else {
                    Some(Ok(Utf8PathBuf::from(path)))
                }
            }
            _ => None,
        })
        .transpose()
}

#[cfg(test)]
mod test {
    use indoc::indoc;
    use itertools::Itertools;
    use pretty_assertions::assert_eq;
    use test_log::test;

    use crate::test_util::copy_of_testdata;
    use crate::workspace::{PackageFilter, Workspace};

    use super::*;

    mod exclude_re_expr_call;
    mod exclude_re_expr_common;
    mod exclude_re_expr_match;
    mod exclude_re_expr_method_call;
    mod exclude_re_expr_struct;
    mod exclude_re_expr_unary;
    mod skip_attr_cfg_attr;
    mod skip_attr_expr_call;
    mod skip_attr_expr_match;
    mod skip_attr_expr_method_call;
    mod skip_attr_expr_struct;
    mod skip_attr_expr_unary;
    mod skip_attr_file;
    mod skip_attr_impl;
    mod skip_attr_impl_item_const;
    mod skip_attr_item_const;
    mod skip_attr_item_static;
    mod skip_attr_trait;
    mod skip_attr_trait_item_const;

    /// The memoized control-flow lookup must agree with a fresh, non-memoized
    /// walk. Nesting a `helper(BLOCK)` call chain many levels deep, where
    /// each level's `helper(...)` argument is the whole nested subtree below
    /// it, exercises exactly the scenario the memo cache is built for: an
    /// outer level's `contains_control_flow` query walks (and caches) every
    /// node below it, and every inner level then re-queries a node the outer
    /// query already covered. A `break`/`return` planted at the bottom of
    /// the chain must still be found through every `helper(...)` level above
    /// it despite the cache, while the sibling `log(...)` statement at each
    /// level — which does not contain the break/return — must still be
    /// judged deletable: a memo that leaked one sibling's flag into another's
    /// cached entry would fail this either by under- or over-reporting.
    #[test]
    fn nested_call_chain_with_deep_control_flow_is_detected_through_the_memo_cache() {
        fn block_at(i: usize, depth: usize, tail: &str) -> String {
            if i == depth - 1 {
                format!("{{ let x{i} = compute({i}); log(x{i}); {tail} }}")
            } else {
                format!(
                    "{{ let x{i} = compute({i}); log(x{i}); helper({}); 0 }}",
                    block_at(i + 1, depth, tail)
                )
            }
        }
        let depth = 40;
        let with_break = format!(
            "fn f(n: i32) -> i32 {{
                loop {{
                    helper({});
                }}
                0
            }}
            fn helper(x: i32) -> i32 {{ x }}
            fn compute(n: i32) -> i32 {{ n }}
            fn log(_n: i32) {{}}
            ",
            block_at(0, depth, "break; 0")
        );
        // Every `helper(...)` statement transitively contains the `break` at
        // the bottom of the chain (it's nested in its own argument), so none
        // of them should be offered as a `DeleteStatement` mutant: deleting
        // one would remove the loop's only exit. The sibling `log(...)` at
        // each level does not contain the `break` and must remain deletable.
        let mutants = mutate_source_str(&with_break, &Options::default()).unwrap();
        let delete_statement_names = mutants
            .iter()
            .filter(|m| m.genre == Genre::DeleteStatement)
            .map(|m| m.name(false))
            .collect_vec();
        assert_eq!(
            delete_statement_names.len(),
            depth,
            "only the {depth} `log(...)` statements should be deletable, got {delete_statement_names:?}"
        );
        assert!(
            delete_statement_names
                .iter()
                .all(|n| !n.contains("helper(")),
            "no `helper(...)` statement should be deletable: each one contains \
             the `break` further down, but got {delete_statement_names:?}"
        );

        // Same chain with `return 0` instead of `break` at the bottom: also
        // must be detected as control flow through every level of the cache.
        let with_return = format!(
            "fn f(n: i32) -> i32 {{
                helper({});
                0
            }}
            fn helper(x: i32) -> i32 {{ x }}
            fn compute(n: i32) -> i32 {{ n }}
            fn log(_n: i32) {{}}
            ",
            block_at(0, depth, "return 0; 0")
        );
        let mutants = mutate_source_str(&with_return, &Options::default()).unwrap();
        let delete_statement_names = mutants
            .iter()
            .filter(|m| m.genre == Genre::DeleteStatement)
            .map(|m| m.name(false))
            .collect_vec();
        assert_eq!(
            delete_statement_names.len(),
            depth,
            "only the {depth} `log(...)` statements should be deletable, got {delete_statement_names:?}"
        );
        assert!(
            delete_statement_names
                .iter()
                .all(|n| !n.contains("helper(")),
            "no `helper(...)` statement should be deletable: each one contains \
             the `return` further down, but got {delete_statement_names:?}"
        );

        // Sanity check: the very same chain with a plain `0` tail (no control
        // flow at all) makes every `helper(...)`/`log(...)` statement
        // deletable, so the assertions above are exercising real detection
        // and not e.g. some unrelated exclusion.
        let without_control_flow = format!(
            "fn f(n: i32) -> i32 {{
                helper({});
                0
            }}
            fn helper(x: i32) -> i32 {{ x }}
            fn compute(n: i32) -> i32 {{ n }}
            fn log(_n: i32) {{}}
            ",
            block_at(0, depth, "0")
        );
        let mutants = mutate_source_str(&without_control_flow, &Options::default()).unwrap();
        let delete_statement_count = mutants
            .iter()
            .filter(|m| m.genre == Genre::DeleteStatement)
            .count();
        // One `helper(...)` and one `log(...)` deletable statement per level.
        assert_eq!(delete_statement_count, depth * 2);
    }

    #[test]
    fn path_ends_with() {
        use super::path_ends_with;
        use syn::parse_quote;

        let path = parse_quote! { foo::bar::baz };
        assert!(path_ends_with(&path, "baz"));
        assert!(!path_ends_with(&path, "bar"));
        assert!(!path_ends_with(&path, "foo"));

        let path = parse_quote! { baz };
        assert!(path_ends_with(&path, "baz"));
        assert!(!path_ends_with(&path, "bar"));

        let path = parse_quote! { BTreeMap<K, V> };
        assert!(path_ends_with(&path, "BTreeMap"));
        assert!(!path_ends_with(&path, "V"));
        assert!(!path_ends_with(&path, "K"));
    }

    /// We should not generate mutants that produce the same tokens as the
    /// source.
    #[test]
    fn no_mutants_equivalent_to_source() {
        let code = indoc! { "
            fn always_true() -> bool { true }
        "};
        let source_file = SourceFile::for_tests("src/lib.rs", code, "unimportant", true);
        let (mutants, _files) = walk_file(&source_file, &Options::default()).expect("walk_file");
        let mutant_names = mutants.iter().map(|m| m.name(false)).collect_vec();
        // It would be good to suggest replacing this with 'false', breaking a key behavior,
        // but bad to replace it with 'true', changing nothing.
        assert_eq!(
            mutant_names,
            ["src/lib.rs: replace always_true -> bool with false"]
        );
    }

    /// We don't visit functions inside files marked with `#![cfg(test)]`.
    #[test]
    fn no_mutants_in_files_with_inner_cfg_test_attribute() {
        let options = Options::default();
        let console = Console::new();
        let tmp = copy_of_testdata("cfg_test_inner");
        let workspace = Workspace::open(tmp.path()).unwrap();
        let discovered = workspace
            .discover(&PackageFilter::All, &options, &console)
            .unwrap();
        assert_eq!(discovered.mutants.as_slice(), &[]);
    }

    /// Helper function for `find_path_attribute` tests
    fn run_find_path_attribute(
        token_stream: &TokenStream,
    ) -> std::result::Result<Option<Utf8PathBuf>, String> {
        let token_string = token_stream.to_string();
        let item_mod = syn::parse_str::<syn::ItemMod>(&token_string).unwrap_or_else(|err| {
            panic!("Failed to parse test case token stream: {token_string}\n{err}")
        });
        find_path_attribute(&item_mod.attrs)
    }

    #[test]
    fn find_path_attribute_on_module_item() {
        let outer = run_find_path_attribute(&quote! {
            #[path = "foo_file.rs"]
            mod foo;
        });
        assert_eq!(outer, Ok(Some(Utf8PathBuf::from("foo_file.rs"))));

        let inner = run_find_path_attribute(&quote! {
            mod foo {
                #![path = "foo_folder"]

                #[path = "file_for_bar.rs"]
                mod bar;
            }
        });
        assert_eq!(inner, Ok(Some(Utf8PathBuf::from("foo_folder"))));
    }

    #[test]
    fn find_path_attribute_matches_the_right_attribute() {
        let mismatched = run_find_path_attribute(&quote! {
            #[not_the_path = "something"]
            mod mismatched;
        });
        assert_eq!(mismatched, Ok(None));
    }

    #[test]
    fn reject_module_path_absolute() {
        // dots are valid
        let dots = run_find_path_attribute(&quote! {
            #[path = "contains/../dots.rs"]
            mod dots;
        });
        assert_eq!(dots, Ok(Some(Utf8PathBuf::from("contains/../dots.rs"))));

        let dots_inner = run_find_path_attribute(&quote! {
            mod dots_in_path {
                #![path = "contains/../dots"]
            }
        });
        assert_eq!(dots_inner, Ok(Some(Utf8PathBuf::from("contains/../dots"))));

        let leading_slash = run_find_path_attribute(&quote! {
            #[path = "/leading_slash.rs"]
            mod dots;
        });
        assert_eq!(leading_slash, Err("/leading_slash.rs".to_owned()));

        let allow_other_slashes = run_find_path_attribute(&quote! {
            #[path = "foo/other/slashes/are/allowed.rs"]
            mod dots;
        });
        assert_eq!(
            allow_other_slashes,
            Ok(Some(Utf8PathBuf::from("foo/other/slashes/are/allowed.rs")))
        );

        let leading_slash2 = run_find_path_attribute(&quote! {
            #[path = "/leading_slash/../and_dots.rs"]
            mod dots;
        });
        assert_eq!(
            leading_slash2,
            Err("/leading_slash/../and_dots.rs".to_owned())
        );
    }

    /// Demonstrate that we can generate mutants from a string, without needing a whole tree.
    #[test]
    fn mutants_from_test_str() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {"
                fn always_true() -> bool { true }
            "},
            &options,
        )
        .expect("walk_file_string");
        assert_eq!(
            mutants.iter().map(|m| m.name(false)).collect_vec(),
            ["src/main.rs: replace always_true -> bool with false"]
        );
    }

    #[test]
    fn skip_unsafe_fn() {
        // Possibly we should actually mutate unsafe fns, but not unsafe blocks?
        // <https://github.com/sourcefrog/cargo-mutants/issues/405>
        let mutants = mutate_source_str(
            indoc! {"
                unsafe fn foo() -> usize {
                    2 + 3
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(mutants, []);
    }

    #[test]
    fn skip_functions_with_test_attribute() {
        let mutants = mutate_source_str(
            indoc! {"
                #[test]
                fn test_function() {
                    println!(\"test\");
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(mutants, []);
    }

    #[test]
    fn skip_functions_with_tokio_test_attribute() {
        let mutants = mutate_source_str(
            indoc! {"
                #[tokio::test]
                async fn tokio_test() {
                    println!(\"test\");
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(mutants, []);
    }

    #[test]
    fn skip_functions_with_sqlx_test_attribute() {
        let mutants = mutate_source_str(
            indoc! {"
                #[sqlx::test]
                async fn sqlx_test() {
                    println!(\"test\");
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(mutants, []);
    }

    #[test]
    fn skip_functions_with_arbitrary_test_attribute() {
        let mutants = mutate_source_str(
            indoc! {"
                #[my_framework::test]
                fn custom_test() {
                    println!(\"test\");
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(mutants, []);
    }

    #[test]
    fn do_not_skip_functions_with_non_test_attributes() {
        let mutants = mutate_source_str(
            indoc! {"
                #[derive(Debug)]
                pub fn testing_something() -> i32 {
                    42
                }

                #[some_crate::test]
                fn some_test() {
                    println!(\"test\");
                }

                #[some_attr]
                pub fn regular_function() -> i32 {
                    100
                }
            "},
            &Options::default(),
        )
        .unwrap();
        // Should have mutants for testing_something and regular_function, but not for some_test
        let mutant_names = mutants.iter().map(|m| m.name(false)).collect_vec();
        assert_eq!(mutant_names.len(), 6); // 3 mutants each for the two non-test functions
        assert!(mutant_names.iter().any(|n| n.contains("testing_something")));
        assert!(mutant_names.iter().any(|n| n.contains("regular_function")));
        assert!(!mutant_names.iter().any(|n| n.contains("some_test")));
    }

    /// Skip mutating arguments to a particular named function.
    #[test]
    fn skip_named_fn() {
        let options = Options {
            skip_calls: vec!["dont_touch_this".to_owned()],
            ..Default::default()
        };
        let mut mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    dont_touch_this(2 + 3);
                }
            "},
            &options,
        )
        .expect("walk_file_string");
        // Ignore the main function itself
        mutants.retain(|m| m.genre != Genre::FnValue);
        assert_eq!(mutants, []);
    }

    #[test]
    fn skip_false_if_condition_that_would_remove_a_loop_exit() {
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    loop {
                        if done() {
                            break;
                        }
                    }
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(
            mutants
                .iter()
                .filter(|m| m.genre == Genre::IfCondition)
                .map(|m| m.name(true))
                .collect_vec(),
            ["src/main.rs:3:12: replace done() with true in main"]
        );
    }

    #[test]
    fn skip_with_capacity_by_default() {
        let options = Options::from_arg_strs(["mutants"]);
        let mut mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    let mut v = Vec::with_capacity(2 * 100);
                }
            "},
            &options,
        )
        .expect("walk_file_string");
        // Ignore the main function itself
        mutants.retain(|m| m.genre != Genre::FnValue);
        assert_eq!(mutants, []);
    }

    #[test]
    fn mutate_vec_with_capacity_when_default_skips_are_turned_off() {
        let options = Options::from_arg_strs(["mutants", "--skip-calls-defaults", "false"]);
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    let mut _v = std::vec::Vec::<String>::with_capacity(2 * 100);
                }
            "},
            &options,
        )
        .expect("walk_file_string");
        dbg!(&mutants);
        // The main fn plus two mutations of the `*` expression.
        assert_eq!(mutants.len(), 3);
    }

    #[test]
    fn skip_method_calls_by_name() {
        let options = Options::from_arg_strs(["mutants", "--skip-calls", "dont_touch_this"]);
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    let mut v = v::new();
                    v.dont_touch_this(2 + 3);
                }
            "},
            &options,
        )
        .unwrap();
        dbg!(&mutants);
        assert_eq!(
            mutants
                .iter()
                .filter(|mutant| mutant.genre != Genre::FnValue)
                .count(),
            0
        );
    }

    #[test]
    fn mutant_name_includes_type_parameters() {
        // From https://github.com/sourcefrog/cargo-mutants/issues/334
        let options = Options::from_arg_strs(["mutants"]);
        let mutants = mutate_source_str(
            indoc! {r#"
            impl AsRef<str> for Apath {
                fn as_ref(&self) -> &str {
                    &self.0
                }
            }

            impl From<Apath> for String {
                fn from(a: Apath) -> String {
                    a.0
                }
            }

            impl<'a> From<&'a str> for Apath {
                fn from(s: &'a str) -> Apath {
                    assert!(Apath::is_valid(s), "invalid apath: {s:?}");
                    Apath(s.to_string())
                }
            }
            "#},
            &options,
        )
        .unwrap();
        dbg!(&mutants);
        let mutant_names = mutants.iter().map(|m| m.name(false) + "\n").join("");
        assert_eq!(
            mutant_names,
            indoc! {r#"
                src/main.rs: replace <impl AsRef<str> for Apath>::as_ref -> &str with ""
                src/main.rs: replace <impl AsRef<str> for Apath>::as_ref -> &str with "xyzzy"
                src/main.rs: replace <impl From<Apath> for String>::from -> String with String::new()
                src/main.rs: replace <impl From<Apath> for String>::from -> String with "xyzzy".into()
                src/main.rs: replace <impl From<&'a str> for Apath>::from -> Apath with Default::default()
            "#}
        );
    }

    #[test]
    fn mutate_match_arms_with_fallback() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    match x {
                        X::A => {},
                        X::B => {},
                        _ => {},
                    }
                }
            "},
            &options,
        )
        .unwrap();
        assert_eq!(
            mutants
                .iter()
                .filter(|m| m.genre == Genre::MatchArm)
                .map(|m| m.name(true))
                .collect_vec(),
            [
                "src/main.rs:3:9: delete match arm X::A in main",
                "src/main.rs:4:9: delete match arm X::B in main",
            ]
        );
    }

    #[test]
    fn mutate_if_condition() {
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    if foo() {
                        bar();
                    }
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(
            mutants
                .iter()
                .filter(|m| m.genre == Genre::IfCondition)
                .map(|m| m.name(true))
                .collect_vec(),
            [
                "src/main.rs:2:8: replace foo() with true in main",
                "src/main.rs:2:8: replace foo() with false in main",
            ]
        );
    }

    #[test]
    fn skip_if_let_condition() {
        // `if let` is not a bool condition, but mutants inside the body are still found.
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    if let Some(x) = foo() {
                        let _ = x > 1;
                    }
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert!(!mutants.iter().any(|m| m.genre == Genre::IfCondition));
        assert!(mutants.iter().any(|m| m.genre == Genre::BinaryOperator));
    }

    #[test]
    fn mutate_while_condition_only_to_false() {
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    let mut n = 10;
                    while n > 0 {
                        n -= 1;
                    }
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(
            mutants
                .iter()
                .filter(|m| m.genre == Genre::WhileCondition)
                .map(|m| m.name(true))
                .collect_vec(),
            ["src/main.rs:3:11: replace n > 0 with false in main"]
        );
    }

    /// Names of the `DeleteStatement` mutants generated from `code`.
    fn delete_statement_mutants(code: &str) -> Vec<String> {
        mutate_source_str(code, &Options::default())
            .unwrap()
            .iter()
            .filter(|m| m.genre == Genre::DeleteStatement)
            .map(|m| m.describe_change().to_owned())
            .collect_vec()
    }

    #[test]
    fn delete_method_call_statement() {
        assert_eq!(
            delete_statement_mutants(indoc! {"
                fn f(v: &mut Vec<u32>) {
                    v.push(1);
                }
            "}),
            ["delete v.push(1); in f"]
        );
    }

    #[test]
    fn skip_deleting_statement_using_an_untyped_local() {
        assert_eq!(
            delete_statement_mutants(indoc! {"
                fn f() {
                    let mut v = Vec::new();
                    v.push(1);
                }
            "}),
            Vec::<String>::new()
        );
    }

    #[test]
    fn delete_statement_using_a_typed_local() {
        assert_eq!(
            delete_statement_mutants(indoc! {"
                fn f() {
                    let mut v: Vec<u32> = Vec::new();
                    v.push(1);
                }
            "}),
            ["delete v.push(1); in f"]
        );
    }

    #[test]
    fn skip_deleting_compound_assignment_macro_and_try_statements() {
        assert_eq!(
            delete_statement_mutants(indoc! {"
                fn f(mut n: u32, x: bool) -> Result<(), Error> {
                    n -= 1;
                    assert!(x);
                    foo()?;
                    Ok(())
                }
            "}),
            Vec::<String>::new()
        );
    }

    #[test]
    fn delete_assignment_to_a_parameter() {
        assert_eq!(
            delete_statement_mutants(indoc! {"
                fn f(mut x: u32) -> u32 {
                    x = 1;
                    x
                }
            "}),
            ["delete x = 1; in f"]
        );
    }

    #[test]
    fn mutate_bool_literal_in_let() {
        let mutants = mutate_source_str(
            indoc! {"
                fn f() {
                    let x = true;
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(
            mutants
                .iter()
                .filter(|m| m.genre == Genre::BoolLiteral)
                .map(|m| m.name(true))
                .collect_vec(),
            ["src/main.rs:2:13: replace true with false in f"]
        );
    }

    #[test]
    fn mutate_early_return_value() {
        let mutants = mutate_source_str(
            indoc! {"
                fn f(n: i32) -> i32 {
                    if n < 0 {
                        return 0;
                    }
                    n * 2
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(
            mutants
                .iter()
                .filter(|m| m.genre == Genre::ReturnValue)
                .map(|m| m.name(true))
                .collect_vec(),
            [
                "src/main.rs:3:16: replace 0 with 1 in f",
                "src/main.rs:3:16: replace 0 with -1 in f",
            ]
        );
    }

    #[test]
    fn skip_return_value_when_body_is_a_single_return() {
        let mutants = mutate_source_str(
            indoc! {"
                fn f() -> i32 {
                    return 7;
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert!(!mutants.iter().any(|m| m.genre == Genre::ReturnValue));
        assert!(mutants.iter().any(|m| m.genre == Genre::FnValue));
    }

    #[test]
    fn skip_return_value_inside_a_closure() {
        let mutants = mutate_source_str(
            indoc! {"
                fn f() -> i32 {
                    let g = || {
                        return 3;
                    };
                    g()
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert!(!mutants.iter().any(|m| m.genre == Genre::ReturnValue));
    }

    #[test]
    fn skip_bool_literal_that_is_the_whole_function_body() {
        let mutants = mutate_source_str(
            indoc! {"
                fn f() -> bool {
                    true
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert!(!mutants.iter().any(|m| m.genre == Genre::BoolLiteral));
        assert!(mutants.iter().any(|m| m.genre == Genre::FnValue));
    }

    #[test]
    fn skip_bool_literal_that_is_a_whole_match_guard() {
        let mutants = mutate_source_str(
            indoc! {"
                fn f(x: Option<u32>) -> u32 {
                    match x {
                        Some(_) if true => 1,
                        _ => 2,
                    }
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert!(!mutants.iter().any(|m| m.genre == Genre::BoolLiteral));
        assert!(mutants.iter().any(|m| m.genre == Genre::MatchArmGuard));
    }

    #[test]
    fn skip_bool_literals_in_patterns_and_attributes() {
        let mutants = mutate_source_str(
            indoc! {r#"
                #[doc = "documentation"]
                fn g(b: bool) -> u32 {
                    match b {
                        true => 1,
                        false => 2,
                    }
                }
            "#},
            &Options::default(),
        )
        .unwrap();
        assert!(!mutants.iter().any(|m| m.genre == Genre::BoolLiteral));
    }

    #[test]
    fn return_same_file_enum_unit_variants() {
        let mutants = mutate_source_str(
            indoc! {"
                enum Colour { Red, Green, Custom(u32) }

                fn pick() -> Colour {
                    Colour::Red
                }
            "},
            &Options::default(),
        )
        .unwrap();
        assert_eq!(
            mutants
                .iter()
                .filter(|m| m.genre == Genre::FnValue)
                .map(|m| m.replacement_text().to_owned())
                .collect_vec(),
            ["Colour::Green"]
        );
    }

    #[test]
    fn skip_match_arms_without_fallback() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    match x {
                        X::A => {},
                        X::B => {},
                    }
                }
            "},
            &options,
        )
        .unwrap();

        let empty: &[&str] = &[];
        assert_eq!(
            mutants
                .iter()
                .filter(|m| m.genre == Genre::MatchArm)
                .map(|m| m.name(true))
                .collect_vec(),
            empty
        );
    }

    #[test]
    fn mutate_match_guard() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    match x {
                        X::A if foo() => {},
                        X::A => {},
                        X::B => {},
                        X::C if bar() => {},
                    }
                }
            "},
            &options,
        )
        .unwrap();
        assert_eq!(
            mutants
                .iter()
                .filter(|m| m.genre == Genre::MatchArmGuard)
                .map(|m| m.name(true))
                .collect_vec(),
            [
                "src/main.rs:3:17: replace match guard foo() with true in main",
                "src/main.rs:3:17: replace match guard foo() with false in main",
                "src/main.rs:6:17: replace match guard bar() with true in main",
                "src/main.rs:6:17: replace match guard bar() with false in main",
            ]
        );
    }

    #[test]
    fn skip_removing_match_arm_with_guard() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {"
                fn main() {
                    match x {
                        X::A if foo() => {},
                        X::A => {},
                        _ => {},
                    }
                }
            "},
            &options,
        )
        .unwrap();
        assert_eq!(
            mutants
                .iter()
                .filter(|m| matches!(m.genre, Genre::MatchArmGuard | Genre::MatchArm))
                .map(|m| m.name(true))
                .collect_vec(),
            [
                "src/main.rs:4:9: delete match arm X::A in main",
                "src/main.rs:3:17: replace match guard foo() with true in main",
                "src/main.rs:3:17: replace match guard foo() with false in main",
            ]
        );
    }

    #[test]
    fn mutate_comparisons() {
        assert_eq!(
            mutate_expr("a > b"),
            &["replace > with ==", "replace > with <", "replace > with >="]
        );
        assert_eq!(
            mutate_expr("a < b "),
            &["replace < with ==", "replace < with >", "replace < with <="]
        );
        assert_eq!(mutate_expr("a == b"), &["replace == with !="]);
        assert_eq!(mutate_expr("a != b"), &["replace != with =="]);
        assert_eq!(
            mutate_expr("a >= b"),
            &["replace >= with <", "replace >= with >"]
        );
        assert_eq!(
            mutate_expr("a <= b"),
            &["replace <= with >", "replace <= with <"]
        );
    }

    #[test]
    fn mutate_binops() {
        assert_eq!(mutate_expr(" a >> 2; "), &["replace >> with <<"]);
        assert_eq!(mutate_expr("a << 2; "), &["replace << with >>"]);
        assert_eq!(mutate_expr("a && b"), &["replace && with ||"]);
        assert_eq!(mutate_expr("a || b"), &["replace || with &&"]);
        assert_eq!(
            mutate_expr("a + b"),
            &["replace + with -", "replace + with *"]
        );
        assert_eq!(
            mutate_expr("a - b"),
            &["replace - with +", "replace - with /"]
        );
        assert_eq!(
            mutate_expr("a * b"),
            &["replace * with +", "replace * with /"]
        );
        assert_eq!(
            mutate_expr("a / b"),
            &["replace / with %", "replace / with *"]
        );
        assert_eq!(
            mutate_expr("a & b"),
            &["replace & with |", "replace & with ^"]
        );
        assert_eq!(
            mutate_expr("a | b"),
            &["replace | with &", "replace | with ^"]
        );
        assert_eq!(
            mutate_expr("a ^ b"),
            &["replace ^ with |", "replace ^ with &"]
        );
    }

    #[test]
    fn mutate_assign_ops() {
        assert_eq!(
            mutate_expr("a += b"),
            &["replace += with -=", "replace += with *="]
        );
        assert_eq!(
            mutate_expr("a -= b"),
            &["replace -= with +=", "replace -= with /="]
        );
        assert_eq!(
            mutate_expr("a *= b"),
            &["replace *= with +=", "replace *= with /="]
        );
        assert_eq!(
            mutate_expr("a /= b"),
            &["replace /= with %=", "replace /= with *="]
        );
        assert_eq!(mutate_expr("a &= b"), &["replace &= with |="]);
        assert_eq!(mutate_expr("a |= b"), &["replace |= with &="]);
        assert_eq!(
            mutate_expr("a ^= b"),
            &["replace ^= with |=", "replace ^= with &="]
        );
        assert_eq!(
            mutate_expr("a %= b"),
            &["replace %= with /=", "replace %= with +="]
        );
        assert_eq!(mutate_expr("a >>= b"), &["replace >>= with <<="]);
        assert_eq!(mutate_expr("a <<= b"), &["replace <<= with >>="]);
    }

    #[test]
    fn delete_struct_field_with_default() {
        let mutants = mutate_expr(
            r#"
            let cat = Cat {
                name: "Felix",
                coat: Coat::Tuxedo,
                ..Default::default()
            };
            "#,
        );
        // Should generate mutants to delete each field
        assert_eq!(
            mutants,
            &[
                "delete field name from struct Cat expression",
                "delete field coat from struct Cat expression"
            ]
        );
    }

    #[test]
    fn skip_struct_without_default() {
        let mutants = mutate_expr(
            r#"
            let cat = Cat {
                name: "Felix",
                coat: Coat::Tuxedo,
            };
            "#,
        );
        // Should not generate any field deletion mutants without a default
        assert_eq!(mutants, [] as [String; 0]);
    }

    #[test]
    fn delete_struct_field_with_custom_default() {
        let mutants = mutate_expr(
            r#"
            let config = Config {
                timeout: 30,
                retries: 5,
                ..base_config
            };
            "#,
        );
        // Should work with any base expression, not just Default::default()
        assert_eq!(
            mutants,
            &[
                "delete field timeout from struct Config expression",
                "delete field retries from struct Config expression"
            ]
        );
    }

    #[test]
    fn delete_struct_field_single_field_with_default() {
        let mutants = mutate_expr(
            r#"
            let point = Point {
                x: 10,
                ..Default::default()
            };
            "#,
        );
        // Should work with a single field too
        assert_eq!(mutants, &["delete field x from struct Point expression"]);
    }

    #[test]
    fn delete_struct_field_complex_values() {
        let mutants = mutate_expr(
            r#"
            let settings = Settings {
                enabled: get_enabled(),
                count: compute_count() + 10,
                ..Settings::default()
            };
            "#,
        );
        // Should work regardless of the complexity of the field values
        // The visitor also generates mutants for expressions within field values
        assert_eq!(
            mutants,
            &[
                "delete field enabled from struct Settings expression",
                "delete field count from struct Settings expression",
                "replace + with -",
                "replace + with *"
            ]
        );
    }

    #[test]
    fn exclude_re_attr_filters_specific_mutants() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {r#"
                #[mutants::exclude_re("with 0")]
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "#},
            &options,
        )
        .unwrap();
        let names: Vec<&str> = mutants.iter().map(Mutant::full_name).collect();
        // "replace add -> i32 with 0" should be filtered out, but
        // "replace add -> i32 with 1", "with -1", and the binary operator
        // mutations on the body should still be present.
        assert!(
            !names.iter().any(|n| n.contains("with 0")),
            "should not contain 'with 0' mutant but got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("with 1")),
            "should still contain 'with 1' fn-value mutant: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("with -1")),
            "should still contain 'with -1' fn-value mutant: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("replace + with")),
            "should still contain binary op mutant: {names:?}"
        );
    }

    #[test]
    fn exclude_re_attr_keeps_all_when_no_match() {
        // A valid regex that doesn't match any generated mutant must have no
        // effect, matching the behaviour of `--exclude-re` on the CLI / in
        // the config file.
        let options = Options::default();
        let mutants_with_attr = mutate_source_str(
            indoc! {r#"
                #[mutants::exclude_re("this_matches_nothing")]
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "#},
            &options,
        )
        .unwrap();
        let mutants_without_attr = mutate_source_str(
            indoc! {"
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "},
            &options,
        )
        .unwrap();
        let names_with: Vec<String> = mutants_with_attr.iter().map(|m| m.name(false)).collect();
        let names_without: Vec<String> =
            mutants_without_attr.iter().map(|m| m.name(false)).collect();
        assert_eq!(
            names_with, names_without,
            "exclude_re pattern that matches no mutants should produce the \
             exact same mutants as no attribute at all"
        );
    }

    #[test]
    fn exclude_re_attr_invalid_regex_returns_error() {
        let options = Options::default();
        let result = mutate_source_str(
            indoc! {r#"
                #[mutants::exclude_re("(unclosed")]
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "#},
            &options,
        );
        assert!(result.is_err(), "invalid regex should produce an error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("invalid regex"),
            "error should mention 'invalid regex': {err_msg}"
        );
    }

    #[test]
    fn exclude_re_attr_without_string_arg_returns_error() {
        // Missing arguments must not silently no-op; they should be reported
        // as malformed so users don't think their exclude is in effect.
        let options = Options::default();
        let result = mutate_source_str(
            indoc! {"
                #[mutants::exclude_re]
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "},
            &options,
        );
        assert!(
            result.is_err(),
            "missing arg should produce an error, got: {result:?}"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("malformed"),
            "error should mention 'malformed': {err_msg}"
        );
    }

    #[test]
    fn exclude_re_attr_with_multiple_args_returns_error() {
        let options = Options::default();
        let result = mutate_source_str(
            indoc! {r#"
                #[mutants::exclude_re("a", "b")]
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "#},
            &options,
        );
        assert!(
            result.is_err(),
            "extra args should produce an error, got: {result:?}"
        );
    }

    #[test]
    fn exclude_re_attr_with_non_string_arg_returns_error() {
        let options = Options::default();
        let result = mutate_source_str(
            indoc! {"
                #[mutants::exclude_re(42)]
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "},
            &options,
        );
        assert!(
            result.is_err(),
            "non-string arg should produce an error, got: {result:?}"
        );
    }

    #[test]
    fn exclude_re_attr_error_includes_source_location() {
        let options = Options::default();
        let result = mutate_source_str(
            indoc! {r#"
                fn first() -> i32 { 1 }

                #[mutants::exclude_re("(unclosed")]
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "#},
            &options,
        );
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        // Error should point at the line of the offending attribute.
        assert!(
            err_msg.contains("src/main.rs:3"),
            "error should include source location pointing at attribute on line 3: {err_msg}"
        );
        // And include the bad pattern so the user can find it quickly.
        assert!(
            err_msg.contains("(unclosed"),
            "error should include the offending pattern: {err_msg}"
        );
    }

    #[test]
    fn exclude_re_attr_on_impl_block_applies_to_methods() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {r#"
                struct Foo;
                #[mutants::exclude_re("replace .* -> bool")]
                impl Foo {
                    fn is_ok(&self) -> bool {
                        true
                    }
                    fn count(&self) -> i32 {
                        1 + 2
                    }
                }
            "#},
            &options,
        )
        .unwrap();
        let names: Vec<&str> = mutants.iter().map(Mutant::full_name).collect();
        // is_ok fn replacement should be excluded, but count fn and binary ops should remain
        assert!(
            !names.iter().any(|n| n.contains("is_ok")),
            "should not contain is_ok mutant but got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("count")),
            "should contain count mutant: {names:?}"
        );
    }

    #[test]
    fn cfg_attr_exclude_re_filters_mutants() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {r#"
                #[cfg_attr(test, mutants::exclude_re("replace .* with 0"))]
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "#},
            &options,
        )
        .unwrap();
        let names: Vec<&str> = mutants.iter().map(Mutant::full_name).collect();
        assert!(
            !names.iter().any(|n| n.contains("with 0")),
            "should not contain 'with 0' mutant but got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("replace + with")),
            "should still contain binary op mutant: {names:?}"
        );
    }

    #[test]
    fn exclude_re_attr_on_trait_block_applies_to_default_methods() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {r#"
                #[mutants::exclude_re("replace .* -> bool")]
                trait Check {
                    fn is_ok(&self) -> bool {
                        true
                    }
                    fn count(&self) -> i32 {
                        1 + 2
                    }
                }
            "#},
            &options,
        )
        .unwrap();
        let names: Vec<&str> = mutants.iter().map(Mutant::full_name).collect();
        assert!(
            !names.iter().any(|n| n.contains("is_ok")),
            "should not contain is_ok mutant but got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("count")),
            "should contain count mutant: {names:?}"
        );
    }

    #[test]
    fn exclude_re_attr_on_mod_block_applies_to_functions() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {r#"
                #[mutants::exclude_re("replace .* -> bool")]
                mod inner {
                    pub fn is_ok() -> bool {
                        true
                    }
                    pub fn count() -> i32 {
                        1 + 2
                    }
                }
            "#},
            &options,
        )
        .unwrap();
        let names: Vec<&str> = mutants.iter().map(Mutant::full_name).collect();
        assert!(
            !names.iter().any(|n| n.contains("is_ok")),
            "should not contain is_ok mutant but got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("count")),
            "should contain count mutant: {names:?}"
        );
    }

    #[test]
    fn exclude_re_attr_inner_file_attribute_applies_to_all_functions() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {r#"
                #![mutants::exclude_re("replace .* -> bool")]

                fn is_ok() -> bool {
                    true
                }
                fn count() -> i32 {
                    1 + 2
                }
            "#},
            &options,
        )
        .unwrap();
        let names: Vec<&str> = mutants.iter().map(Mutant::full_name).collect();
        assert!(
            !names.iter().any(|n| n.contains("is_ok")),
            "should not contain is_ok mutant but got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("count")),
            "should contain count mutant: {names:?}"
        );
    }

    #[test]
    fn exclude_re_attr_inherits_from_outer_scope() {
        let options = Options::default();
        let mutants = mutate_source_str(
            indoc! {r#"
                struct Foo;
                #[mutants::exclude_re("replace .* -> bool")]
                impl Foo {
                    #[mutants::exclude_re("replace .* -> i32")]
                    fn count(&self) -> i32 {
                        1 + 2
                    }
                    fn is_ok(&self) -> bool {
                        true
                    }
                    fn add(&self, a: i32, b: i32) -> i32 {
                        a + b
                    }
                }
            "#},
            &options,
        )
        .unwrap();
        let names: Vec<&str> = mutants.iter().map(Mutant::full_name).collect();
        // is_ok: excluded by impl-level pattern (-> bool)
        assert!(
            !names.iter().any(|n| n.contains("is_ok")),
            "should not contain is_ok mutant but got: {names:?}"
        );
        // count: fn-value excluded by fn-level pattern (-> i32), but also
        // impl-level pattern (-> bool) doesn't affect it; binary ops remain
        assert!(
            !names
                .iter()
                .any(|n| n.contains("replace count") || n.contains("replace Foo::count")),
            "should not contain count fn-value mutant but got: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.contains("replace + with") && n.contains("count")),
            "should contain binary op mutant in count: {names:?}"
        );
        // add: fn-value NOT excluded (impl pattern only covers bool), binary ops remain
        assert!(
            names
                .iter()
                .any(|n| n.contains("add") && n.contains("-> i32")),
            "should still contain add fn-value mutant: {names:?}"
        );
    }
}
