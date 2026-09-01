# cargo-mutants performance profiling

Findings from a profiling pass over cargo-mutants itself. This is a point-in-time
snapshot of measurements, not a standing design contract; see DESIGN.md for
architecture and CONTRIBUTING.md for process.

## Scope and environment

| | |
|---|---|
| Date | 2026-09-01 |
| Version / commit | 27.1.0 / `fe82f18` |
| Host | Apple M4 Pro, 12 cores, macOS (darwin 25.4.0), APFS |
| Toolchain | cargo 1.98.0 |
| Binary | `target/release/cargo-mutants`, built with `CARGO_PROFILE_RELEASE_DEBUG=true` for symbolication |

Tooling: `samply` for CPU sampling, `atos` for symbol resolution (samply's saved
profile stores unsymbolicated addresses), a `$CARGO` shim script to count and
identify subprocess invocations, and controlled scaling experiments on generated
trees. `dtrace` was unavailable (requires sudo).

Two measurement targets were used deliberately, because they exercise different
hot paths:

- **the cargo-mutants tree itself** — 54 source files, 466 KB, 776 mutants,
  1.70 mutants/KB. Representative of a normal project.
- **generated single-file trees** — up to 914 KB in one file, ~25 mutants/KB.
  Exercises per-file scaling. The mutant density is ~15x higher than real code,
  so absolute times here are pessimistic; the *scaling exponent* is the result
  that matters, not the constant.

## Summary

| # | Bottleneck | Where | Evidence | Impact |
|---|---|---|---|---|
| 1 | `Span::extract` is O(file) per mutant candidate → discovery is O(n²) | `span.rs:68`, `mutant.rs:272` | 93.1% of CPU; 4x time per 2x size across 4 doublings | Critical on large files |
| 2 | Dropping the `syn` AST | `visit.rs` (via `syn`) | 36.7% of CPU on a normal tree | High |
| 3 | `outcomes.json` fully rewritten per mutant → O(N²) write volume | `output.rs:200-211` | 1543 B/outcome measured; ~465 MB written per 776-mutant run | High |
| 4 | Redundant second `cargo locate-project` | `workspace.rs:137` | 3 cargo spawns per run; ~140 ms each | ~26% of `--list` |
| 5 | 50 ms fixed subprocess poll interval | `process.rs:27,63` | ~25 ms mean latency per phase | ~39 s per 776-mutant run |
| 6 | Integration tests run full mutation runs where `--list`/`--check` would do | `tests/main.rs` | 924 CPU-s total; 126 s in 7 assert-only tests | ~100 s recoverable |

Recommended order of attack by return on effort: **1 → 3 → 2 → 4 → 6 → 5**.

---

## 1. `Span::extract` makes mutant discovery quadratic

**Severity: critical.** Discovery time grows as the square of source file size.

Measured on generated single-file trees, subtracting the ~400 ms fixed cargo
subprocess cost to isolate cargo-mutants' own work:

| file size | mutants | own time | factor vs previous |
|---|---|---|---|
| 114 KB | 2 850 | 1.23 s | — |
| 228 KB | 5 700 | 4.76 s | x3.88 |
| 457 KB | 11 400 | 21.07 s | x4.43 |
| 914 KB | 22 800 | 80.74 s | x3.83 |

Approximately 4x per doubling across four consecutive doublings is O(n²). A
914 KB file takes **81 seconds merely to `--list` mutants**, with no compilation
involved.

CPU attribution from samply on the 914 KB case, after symbolication — a single
~560-byte contiguous code region accounted for essentially all self time:

```
93.1%   Mutant::original_text        (mutant.rs:272)
 0.8%   drop_glue<syn::ty::Type>
 0.5%   syn visit_item_trait_alias
```

### Mechanism

`Span::extract` (`span.rs:68`) resolves a `(line, column)` span by walking the
entire file **character by character** from the beginning:

```rust
pub fn extract(&self, s: &str) -> String {
    let mut r = String::new();
    let mut line_no = 1;
    let mut col_no = 1;
    for c in s.chars() {          // O(file size), every call
        ...
    }
}
```

`Mutant::original_text` (`mutant.rs:272`) is a thin wrapper over it. It is called
from `styled_parts` (`mutant.rs:255`), which `Mutant::new_discovered`
(`mutant.rs:126`) invokes **eagerly for every candidate** — including candidates
that are subsequently discarded by `exclude_re` and other filters.

Total cost is therefore `O(M_f x S_f)` per file, where `M_f` is the number of
operator-genre mutants in file `f` and `S_f` is its size. Both grow linearly with
file size for a given coding style, hence the quadratic.

`Span::replace` (`span.rs:102`) uses the identical char-walk pattern and has the
same complexity.

### Suggested fix

Two independent changes, either of which helps and which compose well:

1. Precompute a line-start byte offset index once per `SourceFile` (the text is
   already held in an `Arc<String>`, so the index can live beside it) and resolve
   spans by byte slicing instead of scanning. This makes `extract`/`replace` O(span
   length) instead of O(file size).
2. Make the human-readable mutant name lazy, computed after the exclusion filters
   rather than during `new_discovered`, so discarded candidates cost nothing.

### Reproduction

```sh
CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release
# generate a large single-file crate, then:
samply record --save-only -o /tmp/p.json.gz -- \
    ./target/release/cargo-mutants mutants --list -d /path/to/big/tree
# samply stores raw addresses; resolve with:
atos -o target/release/cargo-mutants -arch arm64 -l 0x100000000 0x1000d740c
```

## 2. Dropping the `syn` AST costs 36.7% of CPU on a normal tree

On the cargo-mutants tree itself — i.e. ordinary file sizes, where finding #1 has
not yet blown up — the profile is dominated by something else entirely:

```
36.7%   core::ptr::drop_glue<syn::ty::Type>
10.1%   Mutant::original_text
 5.3%   syn visit_pat / DiscoveryVisitor
 4.7%   core::ptr::drop_glue<syn::attr::Meta>
```

Over a third of CPU time is spent **deallocating** the parsed AST, not parsing or
walking it.

Note that `syn` is configured with the `extra-traits` feature (`Cargo.toml:78`),
which adds `Debug`/`Eq`/`Hash` impls to every `syn` type. A grep found no use of
those traits on `syn` types in `src/`. Worth confirming whether the feature can be
dropped — it affects both generated code size and compile time.

Because the process parses each file, visits it, then drops the AST, one option is
to avoid running destructors for ASTs that are provably dead. This trades memory
for time and must be bounded: discovery is followed by a long test phase, so
leaking every AST for the whole run is not acceptable on large workspaces. Measure
before committing to this.

## 3. `outcomes.json` is rewritten in full after every mutant

`OutputDir::add_scenario_outcome` (`output.rs:209-211`) calls `write_lab_outcome`
after **each** completed scenario, and `write_lab_outcome` (`output.rs:200-206`)
serializes the entire accumulated `Vec` into a freshly truncated file:

```rust
fn write_lab_outcome(&self) -> Result<()> {
    serde_json::to_writer_pretty(
        BufWriter::new(File::create(self.path.join("outcomes.json"))?),
        &self.lab_outcome,
    )
    .context("write outcomes.json")
}
```

Total bytes serialized and written over a run of N mutants is proportional to
`sum(1..N)`, i.e. O(N²).

Measured on `testdata/small_well_tested`: **1543 bytes per outcome**.
Extrapolated to this repo's own 776 mutants, the final file is ~1.2 MB but the run
writes roughly **465 MB** in total — an amplification factor of ~388x, along with
the matching JSON serialization CPU.

Additionally, `output.rs:210` does `scenario_outcome.to_owned()` on a value the
caller already owns — an avoidable deep clone once per mutant.

Suggested fix: append incrementally (JSON Lines), or keep the current format but
throttle rewrites on a timer, writing the complete document once at the end. Take
the owned value instead of cloning.

## 4. Three cargo subprocesses at startup, one of them redundant

`--list` on this repo takes **530 ms**, of which only ~130 ms is cargo-mutants'
own CPU. The remainder is waiting on cargo subprocesses.

Captured with a `$CARGO` shim that logs each invocation:

```
locate-project --workspace                       ~140 ms
metadata --format-version 1 --no-deps ...        ~120 ms
locate-project                                   ~140 ms   <- redundant
```

The second `locate-project` comes from `Workspace::filter_packages`
(`workspace.rs:137`) on the `PackageFilter::Auto` path. The `Metadata` fetched
moments earlier at `workspace.rs:102` already contains `workspace_root` and the
`manifest_path` of every workspace member, which is sufficient to find the package
directory closest to the start directory without spawning cargo again.

Removing it saves ~140 ms, about **26% of `--list` wall time**, on every run.

Already correct, for the record: `no_deps()` is set at `workspace.rs:103`. Measured
locally, `cargo metadata` **with** dependencies costs 330-475 ms versus ~120 ms
without, so this flag is worth 200-350 ms and should not be removed.

## 5. Fixed 50 ms subprocess poll interval

`process.rs:27` defines `WAIT_POLL_INTERVAL = Duration::from_millis(50)`, slept in
the wait loop at `process.rs:63`:

```rust
let process_status = loop {
    if let Some(exit_status) = child.poll()? { break exit_status; }
    console.tick();
    sleep(WAIT_POLL_INTERVAL);
};
```

Child completion is therefore detected up to 50 ms late, ~25 ms on average. With
two phases per mutant (build + test) and 776 mutants, that is roughly **39 seconds
of pure sleep** per run, spread across worker threads.

Replacing the poll with a blocking `wait()` on a dedicated waiter thread (or a
SIGCHLD/self-pipe wakeup) removes this. Note that `console.tick()` is currently
driven by the same loop, so progress-bar refresh must be decoupled as part of the
same change rather than inheriting whatever cadence the new wait mechanism has.

## 6. Test suite

Full suite: **90.5 s wall**, 924 CPU-seconds, 430 tests, 12 cores. Parallel
efficiency is 85% (77.0 s theoretical minimum vs 90.5 s observed) — the suite is
CPU-saturated, not serialization-limited. The 290 unit tests finish in 4.98 s and
are not a problem; the cost is entirely in the `tests/main.rs` CLI integration
suite. 44 tests exceed 5 s, 27 exceed 15 s.

DESIGN.md ("Test performance") already states that CLI tests should prefer
`--list` over running all mutants. Seven tests run a **full** mutation run
(baseline plus every mutant of a 4-mutant testdata crate) but assert only
`.assert().success()` — none inspects which mutants were caught or missed:

| test (`tests/main.rs`) | duration |
|---|---|
| `additional_cargo_test_args` | 18.93 s |
| `cargo_test_arg_option` | 18.34 s |
| `all_features_config_option` | 18.22 s |
| `cargo_test_arg_and_additional_cargo_test_args_combined` | 18.08 s |
| `features_config_option` | 17.83 s |
| `cargo_test_arg_multiple_options` | 17.52 s |
| `additional_cargo_args` | 17.44 s |

Total 126 s. Each verifies only that a flag is plumbed through to the underlying
cargo invocation, which `--check` (build only, no test phase) would establish at
roughly one fifth the cost. Estimated saving ~100 s.

Separately, `cross_package_tests` (60.79 s, the single slowest test) performs five
sequential `cargo mutants` invocations inside one `#[test]` function. It alone sets
the floor on suite wall time, since no amount of additional cores can shorten it.
Splitting it into five `#[test]` functions lets nextest schedule them across cores;
total CPU cost is unchanged and no assertion is lost.

Three tests failed during measurement (`cargo_term_color_env_shows_colors`,
`clicolor_force_shows_in_stdout_and_trace`, `colors_always_shows_in_stdout_and_trace`).
These are sensitive to ambient colour-related environment variables in the
measuring shell and are unrelated to performance.

---

## Investigated and found NOT to be bottlenecks

Recording these explicitly so the same ground is not re-covered.

- **Source tree copying.** `reflink` (copy-on-write) is confirmed active on APFS,
  including with the source under `~/dev` and the destination under `$TMPDIR` in
  `/var/folders` (`reflink_used=true` in `debug.log`). `copy_target` defaults to
  `false` (`options.rs:362`), so `target/` is *not* copied unless explicitly
  requested, and per-job build dir copies run concurrently inside `thread::scope`,
  not serially. **Latent risk:** if reflink ever fails — e.g. `$TMPDIR` on a
  different volume, or some CI containers — the fallback to full byte copies is
  silent, logged only at `debug!` level (`copy_tree.rs:53-60`). Promoting this to
  a one-shot `warn!` would make a severe degradation diagnosable.
- **CPU oversubscription.** Not present. A single `jobserver::Client` is created
  once (`lab.rs:56-61`) and shared by all workers, so total compile-job
  concurrency is capped at NCPU process-wide rather than NCPU per worker. This
  only breaks if a user combines `--jobserver=false` with `--jobs > 1`.
- **Work queue lock.** `lab.rs:219-221` binds no guard variable, so the lock is
  released immediately after `.next()`. It is never held across a build or test.
- **Mutant ordering.** Discovery order is preserved by default (shuffle is
  opt-in, `lab.rs:44-46`) and the default `Sharding::Slice` hands each shard a
  contiguous range, which is what maximises incremental-compile cache hits.
- **Progress bar and log tailing.** `TailFile::last_line` (`tail_file.rs:38-49`)
  reads incrementally from the retained file cursor — it does not re-read from
  offset 0. Rendering is capped at a 250 ms interval (`console.rs:605-610`).
- **Child output handling.** stdout/stderr are attached directly to log file
  handles (`process.rs:88-89`), not pipes, so there is no drain loop and no
  pipe-buffer deadlock risk, and no per-line write from our side.
- **`cargo metadata` invocation count.** Exactly once per run, cached in
  `Workspace` for the whole run; converted once into `Vec<Arc<Package>>`. Later
  uses clone only the `Arc`.
- **Regex and glob compilation.** `RegexSet` and `GlobSet` for the examine/exclude
  filters are compiled exactly once in `Options::new` (`options.rs:350-357`); the
  per-mutant path only calls `is_match`.
- **`--in-diff` filtering.** The diff is parsed once into a per-file `HashMap` of
  sorted changed lines, then binary-searched per mutant — effectively O(mutants),
  not O(mutants x hunks), and never re-parsed.
- **Diff generation.** `Mutant::diff` (`mutant.rs:284`) using the `similar` crate
  is correctly lazy — invoked at apply/output time, not eagerly during discovery.
- **Manifest fixups.** `fix_manifest`/`fix_cargo_config` run once per build dir
  (1 + n_threads times per run), never per mutant, on files of a few KB.
- **Per-scenario argv construction.** `cargo_argv` (`cargo.rs:96`) allocates a
  handful of small `String`s per mutant-phase, negligible beside the compile it
  precedes.
