// Copyright 2026 Martin Pool

//! Which lines does the test suite actually run?
//!
//! A mutant in a line that no test executes can't be caught by any test: the
//! answer is known before it is built, so with `--skip-uncovered` it is
//! reported without spending a build and a test run on it.
//!
//! Coverage is measured by building and running the baseline tests once with
//! `-Cinstrument-coverage`, then reading the counters with the `llvm-profdata`
//! and `llvm-cov` binaries shipped in the Rust toolchain's `llvm-tools`
//! component.

#![warn(clippy::pedantic)]

use std::collections::{HashMap, HashSet};
use std::fs::create_dir_all;
use std::process::Command;

use anyhow::{Context, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::Result;

/// The lines of each source file that the test suite executed at least once.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct LineCoverage {
    /// Keyed by path relative to the workspace root, with the lines that ran.
    covered: HashMap<Utf8PathBuf, HashSet<usize>>,
}

impl LineCoverage {
    /// True if any test executed this line of this file.
    ///
    /// Files that coverage said nothing about are treated as uncovered: they
    /// were either not built or not reached.
    pub fn covers(&self, tree_relative_path: &Utf8Path, line: usize) -> bool {
        self.covered
            .get(tree_relative_path)
            .is_some_and(|lines| lines.contains(&line))
    }

    /// How many files had at least one covered line.
    pub fn covered_files(&self) -> usize {
        self.covered.len()
    }

    /// Build coverage from the JSON that `llvm-cov export` writes.
    ///
    /// Paths in the export are absolute, as `rustc` saw them while compiling,
    /// so they are made relative to `build_dir` to match the paths mutants
    /// carry. `rustc` records the real path, which is not textually the build
    /// dir's path when a parent directory is a symlink (on macOS `/tmp` is a
    /// link to `/private/tmp`, and build dirs live under `/tmp`), so the
    /// canonical form of the build dir is accepted as a prefix too.
    fn from_export_json(json: &str, build_dir: &Utf8Path) -> Result<LineCoverage> {
        let export: Export = serde_json::from_str(json).context("parse llvm-cov export output")?;
        let mut prefixes = vec![build_dir.to_owned()];
        match build_dir.canonicalize_utf8() {
            Ok(canonical) if canonical != build_dir => prefixes.push(canonical),
            Ok(_) => {}
            Err(err) => debug!(%build_dir, ?err, "can't canonicalize build dir"),
        }
        let mut covered: HashMap<Utf8PathBuf, HashSet<usize>> = HashMap::new();
        for data in &export.data {
            for file in &data.files {
                let path = Utf8Path::new(&file.filename);
                let Some(relative) = prefixes
                    .iter()
                    .find_map(|prefix| path.strip_prefix(prefix).ok())
                else {
                    // Dependencies and the standard library are outside the
                    // tree and can't hold mutants.
                    continue;
                };
                let lines: HashSet<usize> = file
                    .segments
                    .iter()
                    .filter(|segment| segment.has_count && segment.count > 0)
                    .map(|segment| segment.line)
                    .collect();
                if !lines.is_empty() {
                    covered
                        .entry(relative.to_owned())
                        .or_default()
                        .extend(lines);
                }
            }
        }
        Ok(LineCoverage { covered })
    }
}

/// One `llvm-cov export` document.
#[derive(Deserialize)]
struct Export {
    data: Vec<ExportData>,
}

#[derive(Deserialize)]
struct ExportData {
    files: Vec<ExportFile>,
}

#[derive(Deserialize)]
struct ExportFile {
    filename: String,
    #[serde(default)]
    segments: Vec<Segment>,
}

/// A coverage segment: `[line, column, count, has_count, is_region_entry, is_gap]`.
///
/// `llvm-cov` writes these as a heterogeneous array, so the fields are taken
/// positionally.
struct Segment {
    line: usize,
    count: u64,
    has_count: bool,
}

impl<'de> Deserialize<'de> for Segment {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Segment, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = <Vec<serde_json::Value>>::deserialize(deserializer)?;
        let as_u64 = |i: usize| fields.get(i).and_then(serde_json::Value::as_u64);
        Ok(Segment {
            line: usize::try_from(as_u64(0).unwrap_or(0)).unwrap_or(usize::MAX),
            count: as_u64(2).unwrap_or(0),
            has_count: fields
                .get(3)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Paths to the LLVM tools that read coverage data.
///
/// These ship with the toolchain in the `llvm-tools` component, next to
/// `rustc`'s target libraries, so they always match the compiler that
/// generated the counters. A copy found on `$PATH` might not.
pub struct LlvmTools {
    profdata: Utf8PathBuf,
    cov: Utf8PathBuf,
}

impl LlvmTools {
    /// Find the tools, or explain how to install them.
    pub fn find() -> Result<LlvmTools> {
        let output = Command::new("rustc")
            .arg("--print")
            .arg("target-libdir")
            .output()
            .context("run rustc --print target-libdir")?;
        if !output.status.success() {
            bail!("rustc --print target-libdir failed: {}", output.status);
        }
        let libdir = Utf8PathBuf::from(
            String::from_utf8(output.stdout)
                .context("rustc printed a non-UTF-8 target-libdir")?
                .trim(),
        );
        let bin = libdir
            .parent()
            .ok_or_else(|| anyhow!("target-libdir {libdir} has no parent"))?
            .join("bin");
        let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
        let profdata = bin.join(format!("llvm-profdata{exe_suffix}"));
        let cov = bin.join(format!("llvm-cov{exe_suffix}"));
        for tool in [&profdata, &cov] {
            if !tool.is_file() {
                bail!(
                    "{tool} not found: --skip-uncovered needs the llvm-tools component, so run `rustup component add llvm-tools`"
                );
            }
        }
        debug!(%profdata, %cov, "found llvm tools");
        Ok(LlvmTools { profdata, cov })
    }

    /// Merge the raw profiles written by the test run into one profdata file.
    fn merge(&self, profraw_dir: &Utf8Path, profdata_path: &Utf8Path) -> Result<()> {
        let mut raws: Vec<Utf8PathBuf> = profraw_dir
            .read_dir_utf8()
            .with_context(|| format!("read {profraw_dir}"))?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path().to_owned())
            .filter(|path| path.extension() == Some("profraw"))
            .collect();
        if raws.is_empty() {
            bail!(
                "no .profraw files were written to {profraw_dir}: the baseline tests ran no instrumented binary"
            );
        }
        raws.sort();
        debug!(n_profraw = raws.len(), "merge coverage profiles");
        let output = Command::new(&self.profdata)
            .arg("merge")
            .arg("-sparse")
            .args(&raws)
            .arg("-o")
            .arg(profdata_path)
            .output()
            .context("run llvm-profdata merge")?;
        if !output.status.success() {
            bail!(
                "llvm-profdata merge failed: {}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    /// Export per-line counters for the given test binaries.
    fn export(&self, profdata_path: &Utf8Path, binaries: &[Utf8PathBuf]) -> Result<String> {
        let output = Command::new(&self.cov)
            .arg("export")
            .arg("--instr-profile")
            .arg(profdata_path)
            .arg("--format=text")
            .args(binaries)
            .output()
            .context("run llvm-cov export")?;
        if !output.status.success() {
            bail!(
                "llvm-cov export failed: {}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8(output.stdout).context("llvm-cov export wrote non-UTF-8 JSON")
    }
}

/// Read the coverage of a completed instrumented test run.
///
/// `profraw_dir` holds the `.profraw` files the run wrote, `binaries` are the
/// test executables it ran, and paths in the result are relative to
/// `build_dir`.
pub fn read_coverage(
    tools: &LlvmTools,
    profraw_dir: &Utf8Path,
    binaries: &[Utf8PathBuf],
    build_dir: &Utf8Path,
) -> Result<LineCoverage> {
    if binaries.is_empty() {
        bail!("no test binaries were built, so coverage can't be measured");
    }
    let profdata_path = profraw_dir.join("merged.profdata");
    tools.merge(profraw_dir, &profdata_path)?;
    let json = tools.export(&profdata_path, binaries)?;
    let coverage = LineCoverage::from_export_json(&json, build_dir)?;
    if coverage.covered_files() == 0 {
        warn!(
            "coverage reported no covered lines within the tree; every mutant will be treated as uncovered"
        );
    }
    Ok(coverage)
}

/// The environment a cargo test run needs to write coverage profiles.
///
/// The directory is created if necessary. `%p` and `%m` in the template give
/// each process and each binary its own file, which matters because a test run
/// starts one process per test binary.
pub fn instrumented_env(
    profraw_dir: &Utf8Path,
    encoded_rustflags: Option<&str>,
) -> Result<Vec<(String, String)>> {
    create_dir_all(profraw_dir).with_context(|| format!("create {profraw_dir}"))?;
    let mut rustflags = encoded_rustflags.unwrap_or_default().to_owned();
    if !rustflags.is_empty() {
        rustflags.push('\x1f');
    }
    rustflags.push_str("-Cinstrument-coverage");
    Ok(vec![
        (
            "LLVM_PROFILE_FILE".to_owned(),
            profraw_dir.join("%p-%m.profraw").to_string(),
        ),
        ("CARGO_ENCODED_RUSTFLAGS".to_owned(), rustflags),
    ])
}

#[cfg(test)]
mod test {
    use camino::Utf8Path;

    use super::{LineCoverage, instrumented_env};

    /// A trimmed copy of what `llvm-cov export --format=text` writes: the
    /// segments are `[line, column, count, has_count, is_region_entry, is_gap]`.
    const EXPORT_JSON: &str = r#"{
        "data": [
            {
                "files": [
                    {
                        "filename": "/build/src/covered.rs",
                        "segments": [
                            [3, 1, 7, true, true, false],
                            [3, 31, 0, false, false, false],
                            [9, 5, 0, true, true, false]
                        ]
                    },
                    {
                        "filename": "/build/src/never_run.rs",
                        "segments": [
                            [4, 1, 0, true, true, false]
                        ]
                    },
                    {
                        "filename": "/home/user/.cargo/registry/src/other-1.0/src/lib.rs",
                        "segments": [
                            [1, 1, 12, true, true, false]
                        ]
                    }
                ]
            }
        ]
    }"#;

    #[test]
    fn export_json_gives_only_lines_with_a_nonzero_count() {
        let coverage = LineCoverage::from_export_json(EXPORT_JSON, Utf8Path::new("/build"))
            .expect("parse export json");
        assert!(coverage.covers(Utf8Path::new("src/covered.rs"), 3));
        // Counted, but never executed.
        assert!(!coverage.covers(Utf8Path::new("src/covered.rs"), 9));
        // A line with no segment at all.
        assert!(!coverage.covers(Utf8Path::new("src/covered.rs"), 4));
    }

    #[test]
    fn files_with_no_covered_line_are_not_covered() {
        let coverage = LineCoverage::from_export_json(EXPORT_JSON, Utf8Path::new("/build"))
            .expect("parse export json");
        assert!(!coverage.covers(Utf8Path::new("src/never_run.rs"), 4));
        assert_eq!(coverage.covered_files(), 1);
    }

    #[test]
    fn files_outside_the_build_dir_are_ignored() {
        let coverage = LineCoverage::from_export_json(EXPORT_JSON, Utf8Path::new("/build"))
            .expect("parse export json");
        assert!(!coverage.covers(
            Utf8Path::new("/home/user/.cargo/registry/src/other-1.0/src/lib.rs"),
            1
        ));
    }

    #[test]
    fn an_unknown_file_is_treated_as_uncovered() {
        let coverage = LineCoverage::default();
        assert!(!coverage.covers(Utf8Path::new("src/anything.rs"), 1));
    }

    #[test]
    fn instrumented_env_appends_to_existing_rustflags() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let profraw_dir = Utf8Path::from_path(dir.path()).expect("utf-8 temp dir");
        let env = instrumented_env(profraw_dir, Some("--cap-lints=allow")).expect("build env");
        let rustflags = env
            .iter()
            .find(|(k, _)| k == "CARGO_ENCODED_RUSTFLAGS")
            .map(|(_, v)| v.as_str())
            .expect("rustflags set");
        assert_eq!(rustflags, "--cap-lints=allow\x1f-Cinstrument-coverage");
        let profile_file = env
            .iter()
            .find(|(k, _)| k == "LLVM_PROFILE_FILE")
            .map(|(_, v)| v.as_str())
            .expect("profile file set");
        assert!(profile_file.ends_with("%p-%m.profraw"), "{profile_file}");
    }

    /// `rustc` records the real path of each source file, which is not
    /// textually the build dir's path when the build dir is reached through a
    /// symlink: on macOS every `/tmp` build dir is such a path.
    #[cfg(unix)]
    #[test]
    fn coverage_matches_a_build_dir_reached_through_a_symlink() {
        let build = tempfile::tempdir().expect("create build dir");
        let holder = tempfile::tempdir().expect("create dir to hold the link");
        let real = Utf8Path::from_path(build.path())
            .expect("utf-8 temp dir")
            .canonicalize_utf8()
            .expect("canonicalize build dir");
        let link = Utf8Path::from_path(holder.path())
            .expect("utf-8 temp dir")
            .join("link-to-build-dir");
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");
        let json = format!(
            r#"{{"data": [{{"files": [
                {{"filename": "{real}/src/lib.rs",
                  "segments": [[2, 1, 3, true, true, false]]}}
            ]}}]}}"#
        );
        let coverage = LineCoverage::from_export_json(&json, &link).expect("parse export json");
        assert!(coverage.covers(Utf8Path::new("src/lib.rs"), 2));
    }
}
