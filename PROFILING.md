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
| Toolchain | cargo 1.98.0. `cargo-nextest` is **not** installed on this machine |
| Binary | `target/release/cargo-mutants`, built with `CARGO_PROFILE_RELEASE_DEBUG=true` for symbolication |

Tooling: `samply` for CPU sampling, `atos` for symbol resolution (samply's saved
profile stores unsymbolicated addresses), a `$CARGO` shim script to count and
identify subprocess invocations, and controlled scaling experiments on generated
trees. `dtrace` was unavailable (requires sudo).

### Measurement discipline

**All timings below were taken on an idle machine and are min-of-N repetitions.**
This matters: an initial pass of this profiling produced wall-clock numbers
inflated by roughly 5x because the measurements overlapped a full release
rebuild saturating all 12 cores (15-minute load average 53). Those numbers were
wrong and have been replaced. Every absolute figure here has been re-taken with
the machine quiet and the load average verified beforehand.

The *shape* results — the quadratic scaling exponent and the CPU attribution
percentages — survived the correction; only the absolute magnitudes changed.
Ratios and profile proportions are inherently more robust to background load
than wall-clock absolutes are.

Two measurement targets are used deliberately, because they exercise different
hot paths:

- **the cargo-mutants tree itself** — 54 source files, 466 KB, 776 mutants,
  1.70 mutants/KB. Representative of a normal project.
- **generated single-file trees** — up to 914 KB in one file, ~25 mutants/KB.
  Exercises per-file scaling. The mutant density is ~15x higher than real code,
  so absolute times there are pessimistic; the *scaling exponent* is the result
  that transfers, not the constant.

## Summary

| # | Bottleneck | Where | Evidence | Impact |
|---|---|---|---|---|
| 1 | `Span::extract` is O(file) per mutant candidate → discovery is O(n²) | `span.rs:68`, `mutant.rs:272` | 88.4% of CPU on a large file; ~4x time per 2x size across 4 doublings | Critical on large files |
| 2 | 50 ms fixed subprocess poll interval | `process.rs:27,63` | ~25 ms mean dead time per scenario-phase | ~39 CPU-idle seconds per 776-mutant run |
| 3 | `outcomes.json` fully rewritten per mutant → O(N²) write volume | `output.rs:200-211` | 1543 B/outcome measured; ~465 MB written per 776-mutant run | High at scale |
| 4 | `syn` AST teardown | `visit.rs`, via `syn` | ~18% of CPU on a normal tree, spread across drop glue | Moderate |
| 5 | Redundant second `cargo locate-project` | `workspace.rs:137` | 3 cargo spawns per run, ~15 ms each | ~15% of `--list` |
| 6 | Integration tests run full mutation runs where `--list`/`--check` would do | `tests/main.rs` | 7 tests assert only `.success()`, verified in source | Developer time |

Recommended order of attack by return on effort: **1 → 3 → 2 → 4 → 5 → 6**.

---

## 1. `Span::extract` makes mutant discovery quadratic

**Severity: critical.** Discovery time grows as the square of source file size.

Measured on generated single-file trees, idle machine, min of 3 runs, with the
cargo metadata/lockfile warmed beforehand so startup cost is not counted as
variable:

| file size | mutants | min time | factor vs previous |
|---|---|---|---|
| 114 KB | 2 850 | 267.6 ms | — |
| 228 KB | 5 700 | 858.7 ms | x3.21 |
| 457 KB | 11 400 | 3 205.4 ms | x3.73 |
| 914 KB | 22 800 | 12 427.0 ms | x3.88 |

The ratio converges on 4x per doubling, which is O(n²). A 914 KB file takes
**12.4 seconds merely to `--list` mutants**, with no compilation involved.

CPU attribution from samply on a 304 KB single-file tree, idle machine, after
symbolication — one function dominates completely:

```
88.4%   Mutant::original_text        (mutant.rs:272)
 1.5%   drop_glue<syn::attr::Meta>
 0.9%   drop_glue<syn::ty::Type>
 0.9%   syn visit_item_trait_alias
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
subsequently discarded by `exclude_re` and other filters.

Total cost is therefore `O(M_f x S_f)` per file, where `M_f` is the number of
operator-genre mutants in file `f` and `S_f` is its size. Both grow linearly with
file size for a given coding style, hence the quadratic.

`Span::replace` (`span.rs:102`) uses the identical char-walk pattern and has the
same complexity.

### Suggested fix

Two independent changes, either of which helps and which compose well:

1. Precompute a line-start byte offset index once per `SourceFile` (the text is
   already held in an `Arc<String>`, so the index can live beside it) and resolve
   spans by byte slicing instead of scanning. This makes `extract`/`replace`
   O(span length) instead of O(file size).
2. Make the human-readable mutant name lazy, computed after the exclusion
   filters rather than during `new_discovered`, so discarded candidates cost
   nothing.

### Reproduction

```sh
CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release
# generate a large single-file crate, then:
samply record --save-only -o /tmp/p.json.gz -- \
    ./target/release/cargo-mutants mutants --list -d /path/to/big/tree
# samply stores raw addresses; resolve with:
atos -o target/release/cargo-mutants -arch arm64 -l 0x100000000 0x1000d740c
```

## 2. Fixed 50 ms subprocess poll interval

`process.rs:27` defines `WAIT_POLL_INTERVAL = Duration::from_millis(50)`, slept
in the wait loop at `process.rs:63`:

```rust
let process_status = loop {
    if let Some(exit_status) = child.poll()? { break exit_status; }
    console.tick();
    sleep(WAIT_POLL_INTERVAL);
};
```

Child completion is detected up to 50 ms late, ~25 ms on average. With two
phases per mutant (build + test) and 776 mutants, that is roughly **39 seconds
of dead time** per run. Spread across parallel workers the wall-clock cost is
smaller — of order 3 s at 12 threads — but it is pure idle latency on every
scenario and it is entirely avoidable.

This is arithmetic from a compile-time constant, so unlike the wall-clock
figures elsewhere it is unaffected by machine load.

Replacing the poll with a blocking `wait()` on a dedicated waiter thread (or a
SIGCHLD/self-pipe wakeup) removes it. Note that `console.tick()` is currently
driven by the same loop, so progress-bar refresh must be decoupled as part of
the same change rather than inheriting the new wait mechanism's cadence.

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

Measured on `testdata/small_well_tested`: **1543 bytes per outcome**. This is a
file-size measurement, so it is unaffected by the load problem described above.
Extrapolated to this repo's own 776 mutants, the final file is ~1.2 MB but the
run writes roughly **465 MB** in total — an amplification factor of ~388x, plus
the matching JSON serialization CPU.

Additionally, `output.rs:210` does `scenario_outcome.to_owned()` on a value the
caller already owns — an avoidable deep clone once per mutant.

Suggested fix: append incrementally (JSON Lines), or keep the current format but
throttle rewrites on a timer and write the complete document once at the end.
Take the owned value instead of cloning.

## 4. `syn` AST teardown

On the cargo-mutants tree itself — ordinary file sizes, where finding #1 has not
yet blown up — `--list` takes **95.5 ms** total and the profile is diffuse:

```
12.9%   drop_glue<syn::attr::Meta>
12.9%   syn visit_pat / DiscoveryVisitor
11.3%   Mutant::original_text
 4.8%   drop_glue<syn::ty::Type>
```

Roughly 18% of CPU goes to **deallocating** the parsed AST rather than parsing or
walking it. This is worth attention but is not the dominant cost it appeared to
be in the contaminated first pass.

`syn` is configured with the `extra-traits` feature (`Cargo.toml:78`), which adds
`Debug`/`Eq`/`Hash` impls to every `syn` type. A grep found no use of those traits
on `syn` types in `src/`. Worth confirming whether the feature can be dropped — it
affects generated code size and compile time.

## 5. Three cargo subprocesses at startup, one of them redundant

`--list` on this repo takes **95.5 ms**. Captured with a `$CARGO` shim that logs
each invocation:

| invocation | cost |
|---|---|
| `locate-project --workspace` | ~15.9 ms |
| `metadata --format-version 1 --no-deps` | ~14.7 ms |
| `locate-project` | ~14.6 ms (redundant) |

The second `locate-project` comes from `Workspace::filter_packages`
(`workspace.rs:137`) on the `PackageFilter::Auto` path. The `Metadata` fetched
moments earlier at `workspace.rs:102` already contains `workspace_root` and the
`manifest_path` of every workspace member, which is enough to find the package
directory closest to the start directory without spawning cargo again.

Removing it saves ~15 ms, about **15% of `--list` wall time**, on every run.

Already correct, for the record: `no_deps()` is set at `workspace.rs:103`.
Measured locally, `cargo metadata` **with** dependencies costs 62.7 ms versus
14.7 ms without, so the flag is worth ~48 ms and should not be removed.

## 6. Test suite

Measured with `cargo test --all-features` (nextest is not installed here, so
per-test timings could not be collected):

- 290 unit tests, in-process: **0.08 s**. Not a problem.
- `tests/main.rs` integration suite, 140 tests: **96.15 s**. This is the cost.
- 5 failures, all environmental rather than performance-related: 3 depend on
  ambient colour env vars (`cargo_term_color_env_shows_colors`,
  `clicolor_force_shows_in_stdout_and_trace`,
  `colors_always_shows_in_stdout_and_trace`) and 2 require nextest to be
  installed (`test_with_nextest_on_small_tree`,
  `unexpected_nextest_error_code_causes_a_warning`).

DESIGN.md ("Test performance") states that CLI tests should prefer `--list` over
running all mutants. Seven tests violate this. Each copies the
`testdata/fails_without_feature` tree and runs a **full** mutation run — baseline
plus every mutant — while asserting only `.assert().success()`. None inspects
which mutants were caught or missed. Verified by reading each test body:

- `additional_cargo_test_args`
- `cargo_test_arg_option`
- `all_features_config_option`
- `cargo_test_arg_and_additional_cargo_test_args_combined`
- `features_config_option`
- `cargo_test_arg_multiple_options`
- `additional_cargo_args`

Each verifies only that a flag is plumbed through to the underlying cargo
invocation. `--check` (build only, no test phase) would establish that at a
fraction of the cost, since the assertion does not depend on mutants actually
being tested.

Separately, `cross_package_tests` performs five sequential `cargo mutants`
invocations inside one `#[test]` function. Splitting it into five `#[test]`
functions would let the runner schedule them across cores; total CPU cost is
unchanged and no assertion is lost.

---

## Investigated and found NOT to be bottlenecks

Recording these explicitly so the same ground is not re-covered.

- **Source tree copying.** `reflink` (copy-on-write) is confirmed active on APFS,
  including with the source under `~/dev` and the destination under `$TMPDIR` in
  `/var/folders` (`reflink_used=true` in `debug.log`). `copy_target` defaults to
  `false` (`options.rs:362`), so `target/` is *not* copied unless explicitly
  requested, and per-job build dir copies run concurrently inside `thread::scope`,
  not serially. **Latent risk:** if reflink ever fails — `$TMPDIR` on a different
  volume, some CI containers — the fallback to full byte copies is silent, logged
  only at `debug!` level (`copy_tree.rs:53-60`). Promoting that to a one-shot
  `warn!` would make a severe degradation diagnosable.
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

## Benchmark harness

`autoresearch.sh` at the repo root measures the items above that can be measured
reliably. It generates its own fixtures (never the live `src/` tree, which would
drift as the crate is optimized), reports `total_ms` over three discovery
workloads as the primary metric, and gates on the unit tests plus exact mutant
counts so a faster-but-wrong build cannot score well.

Baseline at `fe82f18`:

```
METRIC total_ms=1572.7
METRIC list_mixed_ms=71.0
METRIC list_bigfile_ms=1435.6
METRIC list_manyfiles_ms=66.0
METRIC e2e_ms=10704.3
METRIC cargo_spawns=3
METRIC mutants_total=11280
```
