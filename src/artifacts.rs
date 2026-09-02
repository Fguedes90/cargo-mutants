// Copyright 2026 Martin Pool

//! Fingerprint build artifacts to detect equivalent and redundant mutants.
//!
//! This implements "Trivial Compiler Equivalence" (Papadakis, Jia, Harman &
//! Le Traon, ICSE 2015): after building a mutant, compare its compiled test
//! executables, byte for byte, against the unmutated baseline and against
//! every other mutant built so far. If they match, no test run can possibly
//! tell the two apart, so the mutant is either equivalent (matches the
//! baseline) or redundant (matches an earlier mutant), and the test phase
//! can be skipped.
//!
//! For this to be meaningful, the comparison must not be defeated by
//! incidental differences that don't reflect the generated code, such as
//! embedded debug info (which records source line numbers, that necessarily
//! move when a mutation is textually applied) or the path of the build
//! directory. [`crate::cargo::encoded_rustflags`] forces `-Cdebuginfo=0`
//! whenever [`Options::detect_equivalent_mutants`] is set, and build
//! directories are otherwise path-independent, so two builds of identical
//! source produce byte-identical artifacts.

use std::fs::{read, read_dir};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

use camino::Utf8PathBuf;
use tracing::debug;

use crate::{BuildDir, Options};

/// A fingerprint of the test-executable artifacts produced by one build.
///
/// This is a 128-bit digest (two independent 64-bit `SipHash-1-3` runs over
/// the same input) of the sorted `(file_name, contents)` pairs of every test
/// executable found in the build's `deps/` directory.
///
/// # Collision argument
///
/// A collision would report two mutants as producing identical artifacts
/// when they don't, hiding a real (potentially non-equivalent) mutant. Two
/// independent 64-bit hashes of the same bytes give an effective ~128-bit
/// digest; even with a suspiciously bad hash function under adversarial
/// input, this is astronomically larger than the number of comparisons ever
/// made in one run (at most a few thousand mutants, i.e. `~2^12`
/// comparisons), so an accidental collision is not a practical concern here.
/// This is a much cheaper and simpler alternative to depending on a
/// cryptographic hash crate, and correctness only requires that non-equal
/// artifacts almost certainly hash to non-equal fingerprints -- not that the
/// digest be preimage-resistant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(u64, u64);

/// List the test-executable artifacts built for `build_dir`, sorted by path
/// (equivalently, by file name, since they share a parent directory).
///
/// Returns an empty vector, never an error, if the deps directory doesn't
/// exist or can't be listed.
pub(crate) fn test_executables(build_dir: &BuildDir, options: &Options) -> Vec<Utf8PathBuf> {
    let deps_dir = build_dir
        .path()
        .join("target")
        .join(profile_dir_name(options))
        .join("deps");
    let dir_iter = match read_dir(&deps_dir) {
        Ok(iter) => iter,
        Err(err) => {
            debug!(
                ?deps_dir,
                ?err,
                "no deps directory; no test executables found"
            );
            return Vec::new();
        }
    };
    let mut executables = Vec::new();
    for entry in dir_iter {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                debug!(?deps_dir, ?err, "error reading deps directory entry");
                continue;
            }
        };
        let path = entry.path();
        if !is_test_executable(&path) {
            continue;
        }
        match Utf8PathBuf::from_path_buf(path) {
            Ok(utf8_path) => executables.push(utf8_path),
            Err(path) => debug!(?path, "skipping non-UTF-8 build artifact path"),
        }
    }
    executables.sort_unstable();
    executables
}

/// Compute a fingerprint of the test-executable artifacts built in `build_dir`.
///
/// Returns `None` (never an error) if the artifacts can't be found or read,
/// for example because the deps directory doesn't exist. Callers must treat
/// `None` as "unknown, so test the mutant normally": missing information
/// must never be interpreted as "equivalent".
pub fn fingerprint_build_artifacts(build_dir: &BuildDir, options: &Options) -> Option<Fingerprint> {
    let executables = test_executables(build_dir, options);
    if executables.is_empty() {
        debug!(
            ?build_dir,
            "no test executables found; can't fingerprint build artifacts"
        );
        return None;
    }

    let mut artifacts: Vec<(&str, Vec<u8>)> = Vec::with_capacity(executables.len());
    for path in &executables {
        let contents = match read(path) {
            Ok(contents) => contents,
            Err(err) => {
                debug!(?path, ?err, "error reading build artifact");
                return None;
            }
        };
        artifacts.push((path.file_name().unwrap_or(path.as_str()), contents));
    }
    // `executables` is already sorted by full path, which (since every entry
    // shares the same parent directory) is equivalent to sorting by file name.

    let mut first = DefaultHasher::new();
    let mut second = DefaultHasher::new();
    // Perturb the second hasher's initial state so that it's not simply
    // computing the same stream twice.
    second.write_u8(0x5a);
    for (name, contents) in &artifacts {
        name.hash(&mut first);
        contents.hash(&mut first);
        name.hash(&mut second);
        contents.hash(&mut second);
    }
    Some(Fingerprint(first.finish(), second.finish()))
}

/// The subdirectory of `target/` that cargo builds into for the given options.
///
/// Cargo does not give every named profile its own directory: the built-in
/// `dev` and `test` profiles both build into `target/debug`, and `release`
/// and `bench` both build into `target/release`. Any other (custom) profile
/// name is used verbatim as the directory name. `cargo_argv` in `cargo.rs`
/// only ever passes `--profile=<name>` for a custom, non-default profile, so
/// this mirrors that: no profile selected means the default `test` profile,
/// i.e. `target/debug`.
fn profile_dir_name(options: &Options) -> &str {
    match options.profile.as_deref() {
        None | Some("dev" | "test") => "debug",
        Some("release" | "bench") => "release",
        Some(other) => other,
    }
}

/// True if `path` looks like a test executable produced by `cargo test
/// --no-run` or `cargo nextest run --no-run`, as opposed to `.d`, `.rmeta`,
/// `.rlib`, `.o`, shared libraries (e.g. a `dylib`/`.so` proc-macro
/// dependency, which is executable on some platforms but isn't a test
/// binary), or other non-executable build output in `deps/`.
#[cfg(unix)]
fn is_test_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.is_file()
        && metadata.permissions().mode() & 0o111 != 0
        && !matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("d" | "rmeta" | "rlib" | "o" | "dylib" | "so")
        )
}

#[cfg(windows)]
fn is_test_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.is_file() && path.extension().and_then(|e| e.to_str()) == Some("exe")
}

#[cfg(test)]
mod test {
    use camino::Utf8Path;
    use std::fs::{create_dir_all, write};

    use tempfile::TempDir;

    use super::*;
    use crate::workspace::Workspace;

    fn build_dir_with_deps(deps: &[(&str, &[u8], bool)]) -> (TempDir, BuildDir) {
        let tmp = TempDir::new().unwrap();
        let tmp_path: &Utf8Path = tmp.path().try_into().unwrap();
        write(
            tmp_path.join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        create_dir_all(tmp_path.join("src")).unwrap();
        write(tmp_path.join("src/lib.rs"), "").unwrap();
        let deps_dir = tmp_path.join("target/debug/deps");
        create_dir_all(&deps_dir).unwrap();
        for (name, contents, executable) in deps {
            let p = deps_dir.join(name);
            write(&p, contents).unwrap();
            set_executable(p.as_std_path(), *executable);
        }
        let workspace = Workspace::open(tmp_path).unwrap();
        let build_dir = BuildDir::in_place(workspace.root()).unwrap();
        (tmp, build_dir)
    }

    #[cfg(unix)]
    fn set_executable(path: &std::path::Path, executable: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(windows)]
    fn set_executable(_path: &std::path::Path, _executable: bool) {
        // Windows has no executable bit; executables are identified by extension.
    }

    #[test]
    fn missing_deps_directory_returns_none() {
        let tmp = TempDir::new().unwrap();
        let tmp_path: &Utf8Path = tmp.path().try_into().unwrap();
        write(
            tmp_path.join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        create_dir_all(tmp_path.join("src")).unwrap();
        write(tmp_path.join("src/lib.rs"), "").unwrap();
        let workspace = Workspace::open(tmp_path).unwrap();
        let build_dir = BuildDir::in_place(workspace.root()).unwrap();
        assert_eq!(
            fingerprint_build_artifacts(&build_dir, &Options::default()),
            None
        );
    }

    #[test]
    fn identical_test_executable_contents_produce_equal_fingerprints() {
        let exe_name = if cfg!(windows) {
            "a-1234.exe"
        } else {
            "a-1234"
        };
        let (_tmp_a, build_dir_a) = build_dir_with_deps(&[(exe_name, b"same bytes", true)]);
        let (_tmp_b, build_dir_b) = build_dir_with_deps(&[(exe_name, b"same bytes", true)]);
        let options = Options::default();
        let fp_a = fingerprint_build_artifacts(&build_dir_a, &options);
        let fp_b = fingerprint_build_artifacts(&build_dir_b, &options);
        assert!(fp_a.is_some());
        assert_eq!(fp_a, fp_b);
    }

    #[test]
    fn different_test_executable_contents_produce_different_fingerprints() {
        let exe_name = if cfg!(windows) {
            "a-1234.exe"
        } else {
            "a-1234"
        };
        let (_tmp_a, build_dir_a) = build_dir_with_deps(&[(exe_name, b"same bytes", true)]);
        let (_tmp_b, build_dir_b) = build_dir_with_deps(&[(exe_name, b"different bytes", true)]);
        let options = Options::default();
        let fp_a = fingerprint_build_artifacts(&build_dir_a, &options);
        let fp_b = fingerprint_build_artifacts(&build_dir_b, &options);
        assert_ne!(fp_a, fp_b);
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_and_intermediate_files_are_ignored() {
        let (_tmp, build_dir) = build_dir_with_deps(&[
            ("a-1234", b"exe bytes", true),
            ("a-1234.d", b"dep info, differs every build", false),
            ("liba-5678.rlib", b"rlib bytes", false),
            ("liba-5678.rmeta", b"rmeta bytes", false),
            ("a-1234.o", b"object bytes", false),
            // A proc-macro dependency's shared library is executable on
            // Unix, but is not a test binary and must be ignored too.
            ("libmutants-9abc.dylib", b"dylib bytes", true),
        ]);
        let options = Options::default();
        let fp = fingerprint_build_artifacts(&build_dir, &options);
        assert!(fp.is_some());

        // A second build dir with only the executable, and different (but
        // ignored) auxiliary file contents, must fingerprint the same.
        let (_tmp2, build_dir2) = build_dir_with_deps(&[
            ("a-1234", b"exe bytes", true),
            ("a-1234.d", b"totally different dep info", false),
        ]);
        assert_eq!(fp, fingerprint_build_artifacts(&build_dir2, &options));
    }

    #[test]
    fn dev_and_test_profiles_use_the_debug_directory() {
        assert_eq!(profile_dir_name(&Options::default()), "debug");
        let mut options = Options {
            profile: Some("dev".to_owned()),
            ..Default::default()
        };
        assert_eq!(profile_dir_name(&options), "debug");
        options.profile = Some("test".to_owned());
        assert_eq!(profile_dir_name(&options), "debug");
    }

    #[test]
    fn release_and_bench_profiles_use_the_release_directory() {
        let mut options = Options {
            profile: Some("release".to_owned()),
            ..Default::default()
        };
        assert_eq!(profile_dir_name(&options), "release");
        options.profile = Some("bench".to_owned());
        assert_eq!(profile_dir_name(&options), "release");
    }

    #[test]
    fn custom_profile_name_is_used_as_the_directory_name() {
        let options = Options {
            profile: Some("mutants".to_owned()),
            ..Default::default()
        };
        assert_eq!(profile_dir_name(&options), "mutants");
    }
}
