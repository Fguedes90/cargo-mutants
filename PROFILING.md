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

## Status: what has since been fixed

The findings above describe the tree at `fe82f18`. A follow-up optimisation pass
addressed several of them; this section records what changed so the table above
is not read as a description of current behaviour.

| # | Finding | Status |
|---|---|---|
| 1 | `Span::extract` quadratic | **Fixed.** `span::LineIndex` caches per-file line-start byte offsets on `SourceFile`; span resolution is O(span) instead of O(file). Single 914 KB file: 1404 → 113 ms. |
| 2 | 50 ms subprocess poll interval | **Fixed on Unix.** The child is now reaped by a dedicated waiter thread and the main loop blocks on a channel with the tick interval as its timeout, so exit is observed at once: measured ~2-9 ms instead of a uniform 0-50 ms. Windows still polls: `std` offers no wait-with-timeout for `Child` and adding a Win32 dependency was not worth it. |
| 3 | `outcomes.json` rewritten per mutant | **Fixed.** Rewrites are throttled to at most one per 500 ms, and `finish()` still writes the complete document, so an interrupted run is at most that stale. The file format is unchanged. |
| 4 | `syn` AST teardown | Open, and now a larger share of what remains: `syn` parsing alone is ~23% of a large-file discovery. Inherent to using `syn`. The `extra-traits` feature **cannot** be dropped: `src/fnvalue.rs` compares `Option<&syn::Type>` values with `==` in ~14 places and `src/visit.rs` logs `?attr` in two `trace!` calls, all of which need the impls it provides. |
| 5 | Redundant `cargo locate-project` | **Fixed, and then some.** Both spawns are gone: `cargo metadata` already reports `workspace_root`, and the enclosing package directory is now found by walking up for a `Cargo.toml` instead of shelling out. 3 cargo spawns per discovery run → 1. |
| 6 | Integration tests run full mutation runs | **Partly fixed.** Three runs that asserted only success were downgraded to `--check`/`--list`, saving 36.9 s of the ~96 s suite (`cross_package_tests` alone went 41.1 → 7.6 s). The rest were examined and must stay: their `.success()` is only reachable if the mutants are really built and caught. |

Four further bottlenecks were found and fixed during that pass, none of which
are in the table above because they only became visible once #1 stopped
dominating the profile:

- **A regex automaton was constructed per visited item.** `push_exclude_re`
  pushed a `RegexSet::empty()` for every fn/impl/mod/expr scope that had no
  `#[mutants::exclude_re]` attribute — the common case — and each empty entry
  was then scanned for every mutant. Nothing is pushed now.
- **Each mutant's name was built three times**: once precomputed at
  construction, again in `allows_mutant` (even when no name filter is
  configured), and again in `list_mutants`. The expensive half (`styled_parts`)
  now runs once per mutant and the rest is composed from a cached description.
- **`tree_relative_slashes()` reallocated the path string on every call**,
  twice per mutant, though it is constant per file. Cached on `SourceFile`.
- **`proc_macro2`'s source map is a thread-local that grows per parsed file**
  and is scanned on every span lookup, making discovery superlinear in *file
  count*. Each file is now parsed on its own thread, and the files in a
  breadth-first level are walked concurrently. 1600-file tree: 588 → 232 ms.

Measured on a fixed three-fixture `--list` benchmark (40-file mixed tree,
one 304 KB file, 201-file tree; 11,280 mutants total), the sum of the three
wall times went from 1548.8 ms to 127.1 ms, a 12.2x reduction. Mutant discovery
output is byte-identical throughout, including `--list --json`.

Of the 127 ms that remain, roughly 57 ms is startup floor that cargo-mutants
does not control: `cargo --version` alone costs 17.7 ms against 16.8 ms for
`cargo metadata --no-deps`, so that cost is cargo's process startup rather than
metadata work, and `--offline` and `--frozen` do not change it. In what is left,
`syn` parsing is 42% of a large-file discovery and the rest is spread across
items no larger than ~9%.

### A later pass: what is left after the allocation work

A further pass profiled the tree again once the items above were fixed. The
profile had changed shape completely: `malloc` was the single largest entry in
self time (37.6% on a large single file, 59.8% aggregated across threads on a
multi-file tree), so the remaining wins were about not allocating rather than
about algorithmic complexity.

| Change | Mechanism | Effect |
|---|---|---|
| Mutant descriptions are built through one callback | `describe_change` rendered the description by building a `Vec<StyledObject<String>>` and then a second `String` per part, ~12 allocations per mutant, only to concatenate them. The parts are now emitted to a callback as `&str`, so the plain rendering allocates only its output string; the coloured rendering styles the same parts, so the two cannot drift. | -8.6% of the three-fixture total |
| `--list` writes names into the output buffer | `list_mutants` called `name(show_line_col)`, which cloned the cached name into a fresh `String` per mutant, and then copied it again into the output. It now appends directly. | included above |
| The pretty-printer recurses into one buffer | `to_pretty_string` allocated a fresh `String::with_capacity(200)` for every nested token group and copied it into its parent, so a type like `Result<Option<Vec<String>>, E>` allocated once per level. Groups now write into the caller's buffer, with every spacing test made relative to the group's start offset so that group-local formatting — including the `Delimiter::None` case, which emits no opening character — is unchanged. | -2.6% |
| `[profile.release]` gets `lto = "thin"`, `codegen-units = 1` | The stock release profile cannot inline across codegen units or into `syn`/`proc-macro2`, which is where most of the remaining time goes. | -2.7% |

The lazy mutant name was also being forced for every candidate: `collect_mutant`
passed `mutant.full_name()` as an argument to `excluded_by_attr_re`, so it was
evaluated before that function could apply its cheap "no `exclude_re` in scope"
test. The name is built only when an attribute is actually in scope now. This
does not show in the `--list` benchmark, where the name is needed for output
anyway, but it matters for runs that filter mutants out.

Three changes were measured and **rejected**, which is worth recording so they
are not tried again:

- **`Mutant` holding `Arc<SourceFile>`** instead of cloning the `SourceFile`
  (two path allocations) per mutant. Interleaved min-of-25: the large-file
  workload improved by 1.2%, but the multi-file workloads got worse by a
  similar or larger amount, twice, on separate days. Net worse.
- **Fat LTO.** Worth 0.9 ms over thin, all of it on one workload, for roughly
  6x the link time. Not worth imposing on everyone who builds from source.
- **`Ident::to_string()` instead of `to_pretty_string()`, and skipping the
  untyped-locals set for blocks with no statement-expression.** Both are
  strictly less work, and neither was measurable: an interleaved A/B moved one
  workload +5.1 ms and another -2.2 ms, which is code layout under LTO, not the
  change.

What remains is close to a floor that does not belong to cargo-mutants. Of the
~122 ms the three fixtures now take, about 57 ms is three `cargo metadata`
spawns (19 ms each; `cargo-mutants --list` on an empty crate costs 22.3 ms
against cargo's own 19.0 ms) and about 10 ms is clap building its command tree
(`cargo-mutants --version`, which does no work at all, costs 4.5 ms). Most of
the rest is `syn` parsing and the AST teardown behind it.

### The largest remaining cost was not in discovery at all

The passes above measured `--list`, which does not generate diffs. Widening the
benchmark to everything cargo-mutants does before the first test runs — that
is, adding `--list --diff`, since `mutants.out/mutants.json` carries a unified
diff for every mutant and is written before any of them is tested — found a
cost an order of magnitude larger than all of discovery:

| Workload | Before | After |
|---|---|---|
| 300 KB file, 7,600 mutants | 4531 ms | 114 ms |
| 40-file mixed tree, 2,040 mutants | 42.5 ms | 45.2 ms |

`Mutant::diff` called `TextDiff::from_lines(whole_file, whole_mutated_file)`,
so `similar` tokenized and hashed every line of both sides for every mutant,
and `mutated_code()` allocated a full copy of the file just to be diffed. The
cost therefore grew with *mutants x file size*: `tokenize_lines` was 55.6% of
self time on the large fixture, with another ~20% in SipHash and 8% in
hashbrown, and the profile was dominated by work whose result was discarded.

This is the same shape as finding #1 above — `Span::extract` making discovery
quadratic — relocated into the diff path, where every real run pays it.

Only the mutated span plus the context radius can appear in a unified diff;
lines further away are identical on both sides. `diff` now builds just that
window, diffs it, and renumbers the hunk headers to the positions a whole-file
diff would have reported. The mutated side is built for the window alone, so
the whole mutated file is no longer materialised to be diffed — which also
removed the second `mutated_code()` call per tested mutant that the earlier
notes flagged.

The equivalence is not obvious and is worth stating: windowing could change
which alignment `similar` picks when lines repeat. It was verified byte-for-byte
on 46 `testdata` trees, cargo-mutants' own tree (2.27 MB of diffs), three
generated fixtures (13.2 MB), hand-built edge cases (mutation on the first and
last line, no trailing newline, CRLF, multi-byte characters before the span, a
file shorter than the context radius, a 69 KB multi-hunk body), and 300
generated sources built from a deliberately tiny pool of repeated lines so that
the alignment is ambiguous (3.88 MB of diffs). Zero mismatches. The property is
pinned by `windowed_diff_matches_whole_file_diff`, which compares the windowed
diff against a whole-file diff for every mutant of six edge-case sources; an
off-by-one in either window boundary, or dropping the renumbering, fails it.

After this, the large-file fixture's profile collapsed from 44,875 samples to
132, and `--list --diff` on it is no longer dominated by anything of ours.

### Memory: the output was being built twice over

Time is not the only cost. Listing a 300 KB file with 7,600 mutants as JSON
peaked at **140 MB**, against 36 MB for the same tree without `--json`:

| Change | Mechanism | Peak |
|---|---|---|
| Baseline | | 140 MB |
| Serialize one mutant at a time; stop caching diffs | `mutants_to_json_string` collected a `serde_json::Value` for *every* mutant — each copying that mutant's cached diff into the DOM — and only then rendered the list. A `MutantsJson` wrapper now `collect_seq`s over the same per-mutant `Value`s, so one is alive at a time, and `mutants.json` is written with `to_writer_pretty` straight into the file. `Mutant` also no longer holds its diff in a `OnceLock`. | 51 MB |
| Stream the list to stdout | `list_mutants` returned a `String` that `main` printed, so the full 17.7 MB rendering was materialised, and `serde` grows that buffer by doubling. `write_mutants` now takes an `impl Write`. | 34 MB |

Note what the middle row does *not* do: serialize `Mutant` directly. Its keys
come out sorted only because they pass through a `serde_json::Value`, whose map
is a `BTreeMap`, while the `Serialize` impl declares them in a different order
— and `outcomes.json` streams the shared `Span`/`LineColumn` impls in
declaration order. Skipping the `Value` would silently reorder `mutants.json`.
The per-mutant `Value` is therefore deliberate, and both formats are pinned by
SHA in the benchmark.

Dropping the diff cache is only correct *because* of the windowing change
above. Retaining every diff bought avoiding a 4.5 s recomputation; it now buys
avoiding 7.7 µs, while holding the whole tree's rendering in memory for the
life of the run.

### Where the remaining memory goes, and why it is not ours

Scaling experiments on generated trees, separating the three terms rather than
letting them move together:

| Term | Cost |
|---|---|
| Empty process | 2.4 MB |
| Each parsed function | **~12.5 KB** |
| Each mutant | 771 bytes |
| Each byte of source | negligible — padding a file from 51 KB to 651 KB of comments moved peak by 1.3 MB |

The dominant term is `syn`'s AST, by a factor of 16 over our own per-mutant
data: a 278 KB file costs 52 MB to parse *with every function skipped, so with
zero mutants*. A function whose source is 68 bytes costs ~12.5 KB of AST. That
is the parser's shape, not something cargo-mutants can restructure, and it is
multiplied by the number of files walked concurrently — the same per-file
threading that made discovery 12x faster. Reducing it would mean giving that
back.

Our own 771 bytes per mutant is a 288-byte `Mutant` plus the slack of the
growing `Vec` and about five small strings. Everything shareable is already
behind an `Arc` (`Package`, the source text, the `LineIndex`). Boxing the
48-byte `Option<MutationTarget>`, or the rejected `Arc<SourceFile>`, would
together save maybe 1-2 MB of the 33 MB — under 5%, for a structural change to
the core type and, in the `Arc` case, a measured time regression.

### The run loop was measured and left alone

`e2e_ms` is rustc-dominated and ±5%, so it cannot resolve per-scenario
overhead. Driving a full 2,040-mutant run with a no-op `$CARGO` shim (which is
honoured, so the loop runs with rustc removed) took 16.18 s against a measured
13.11 s floor for the same 4,080 bare `fork`/`exec` calls. That leaves **~1.5 ms
per mutant** of our own code, and its profile is ~68% `libsystem_kernel`
spawn/wait with nothing algorithmic above it. Against seconds of rustc per real
mutant that is around 0.05%.

`outcomes.json`, which is rewritten from the whole accumulated `LabOutcome`
after every scenario and would otherwise be O(N²) in bytes, is already
throttled to one rewrite per 500 ms.

The one item in the loop that is genuinely ours and looked worth a second
glance — `start_scenario` doing file I/O under a single run-wide output mutex,
plus a whole-file write to apply each mutant and another to revert it — sits
inside that ~1.5 ms. It is recorded here as measured and rejected, not as a
lead.

### Measurement discipline, again

The many-small-files workload has a broad, right-skewed wall-time distribution
— 37.7 to 46.2 ms for one fixed binary, median 42.9 — because it spawns a
thread per file. A min-of-5 on that does not converge, and it produced two
apparent results that were really draws from the same distribution. The
benchmark now takes 21 repetitions and reports the median alongside the
minimum, so that a build whose distribution has actually shifted can be told
apart from one that drew a lucky minimum.


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
