# Changelog

All notable changes to `embassy-supervisor-tools` are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.9.0] - 2026-11-11

Pins `embassy-supervisor-syntax` 0.7.0.

### Added

- A `cli` feature (default) carries the binaries, `inputs`, `out` and `model_json`
  and their `flate2`/`serde_json` dependencies; without it the library parses,
  models, renders and lints from source text alone.
- `--json` carries a top-level `"schema": 1`, a `"fragment"` on each node and pool,
  and a `"cfg"` array on each graph.
- `#[cfg]` attributes on a `supervisor_graph!`/`supervisor_fragment!`/`compose_graph!`
  invocation are read into `Decl::cfg`: a gated graph shows its predicate in the
  diagram title, and fragment declarations of one name under different `#[cfg]` sets
  are all spliced, each item gated by its fragment's predicates.
- An option value may be given as `--opt=value`.
- LICENSE-MIT and LICENSE-APACHE ship in the package.

### Changed

- **Breaking:** `inputs::gather` takes no tool name and prints nothing;
  `collect_inputs`, `crate_default_inputs`, `dependency_sources` and `expand_modules`
  take a warnings sink, and `expand_modules` takes `is_root`. `Sources` and `Scan`
  carry `warnings`; `Decl` derives `Clone` and carries `cfg`, `crate_dir` and
  `fragment_crate_dirs`.
- Inputs are stored as absolute paths; diagnostics and diagrams show them relative to
  the working directory. `--deps` drops a file already given as a graph source.
- `--deps` runs `cargo metadata --no-deps` and scans only the workspace packages the
  current package reaches through `path` dependencies (every member from a virtual
  manifest's directory).
- A file that does not parse is reported, not skipped: `supervisor-mermaid --check` and
  `supervisor-lint` fail on it.
- A directory named `target` is skipped only when it carries Cargo's `CACHEDIR.TAG`; an
  unreadable directory is reported.
- `--html` pages pin mermaid 11.17.2 and `@mermaid-js/layout-elk` 0.2.3, and load ELK
  only for a diagram that asks for it.
- `--update` leaves an unchanged file untouched, writes through a temporary file and a
  rename, keeps the file's CRLF line endings, ignores markers inside fenced code blocks,
  and refuses a second start marker after the managed block.
- `--links` substitutes `{file}` with the absolute path, symlinks unresolved.
- `--watch` re-gathers its inputs every cycle, so a new file or `mod` is picked up.
- `impl` blocks are not scanned for `#[dataflow]` fns: the attribute is rejected on a
  method.

### Fixed

- A resource edge label with a `#[cfg]` predicate is escaped for Mermaid.
- `supervisor-lint`: an array element no longer inherits a sibling element's sides
  through the whole array, and every `--allow` entry matching a signal counts as used.
- `use` imports resolve per inline module, from the access's own module outward.
- A `crate::` binding never binds a fn of another crate when both crate directories are
  known.
- A `src/bin/*.rs` root finds its `mod` files beside it, and `mod b;` inside an inline
  `mod a { .. }` is followed to `a/b.rs`.
- An option given another option as its value (`--title --no-title`) is an error.
- `mmdc` is run as `mmdc.cmd` on Windows.
- A signal's short label keeps its index whole (`LEDS[led::STATUS]`, not `STATUS]`): it
  is cut at the last `::` before the index.
- A fragment declaration that sets an `observe` default or a `default executor` is
  rejected, as the macro rejects it. Through `embassy-supervisor-syntax`, a graph the
  build rejects for its pool bounds, a pool `resources:`/`state:` without `task:`, a
  repeated dep or a `ready_on_write` without a matching `observed beat` write is
  rejected here too.
- A signal one `#[dataflow]` fn accesses at several call sites is discovered under the
  sites' combined condition and the strongest verb: an ungated site makes the edge
  ungated, and a `beat_*` site makes it a beat edge, where the first site alone decided.

## [0.8.0] - 2026-11-07

Pins `embassy-supervisor-syntax` 0.6.0.

### Changed

- **Breaking:** `scan_source`, `scan_fns`, `scan_bundles` and `scan_gate_statics` take a
  parsed `&syn::File`; `scan_fns` fills `Vec<ScannedFn>` and `coverage_warnings` takes
  `&[ScannedFn]`; `Discovered::module` is a `Vec<String>`; `ScannedFn`, `Discovered` and
  `Bundle` carry `rooted` and `crate_dir`; `expand_bundles` is crate-private; the
  `scan_dataflow` re-export is removed.
- **Breaking:** `supervisor-lint` exits 2 on errors.
- The runtime diagram's header comment is `%% runtime coupling:`.

### Fixed

- A `name::fn` binding reaches a crate-root fn only when the scanned file's crate
  directory (the one above `src/`, `-` read as `_`) is `name`, so `imu::run` no longer
  binds the root `fn run` beside it; a `crate::` binding matches a file under `src/`
  exactly.
- An edge drawn from several accesses is labelled with the shortest `#[cfg]` set.
- `supervisor-mermaid --list`/`--check`/`--json` name the output flags they return
  before using.

## [0.7.0] - 2026-10-24

Pins `embassy-supervisor-syntax` 0.5.0.

### Added

- `enabled_if:` and `restart_budget:` reach the typed model
  (`NodeModel::enabled_if: Option<EnabledIfModel>`,
  `NodeModel::restart_budget: Option<RestartBudgetModel>`), the JSON model
  (`enabled_if` as the predicate's text, `restart_budget` as `{max, window_ms}`,
  each with a `*_cfg` list) and the diagrams (`if PREDICATE` and
  `restarts MAX/WINDOWms` node facts, with any `#[cfg]` predicate).

## [0.6.0] - 2026-10-24

### Changed

- Pins `embassy-supervisor-syntax` 0.5.0. Its new `enabled_if:` and `restart_budget:`
  node clauses parse in declarations read from source but are not yet part of the
  JSON model or the diagrams.

### Removed

- `model::NodeModel::pool_size`, the `pool_size` field of the JSON node model and the
  `×N` node-label fact, with the clause.

## [0.5.0] - 2026-09-04

Pins `embassy-supervisor-syntax` 0.4.0.

### Added

- `default executor`: added `default` and `executor_defaulted` flags to the JSON model.
  `--executors` now nests inheriting nodes under their slot and omits `@NAME` from their
  labels, since the inherited tier is shown once at the top of the graph.

## [0.4.1] - 2026-09-03

Pins `embassy-supervisor-syntax = "=0.3.1"`.

### Fixed

- A fn whose `#[dataflow]` is applied through `cfg_attr` was reported as
  "no `#[dataflow]` fn among the scanned files" and its derived edges left
  undrawn. `supervisor-mermaid` and `supervisor-lint` now scan it; the
  `cfg_attr` predicate draws on its edges like a `#[cfg]` on the fn.

## [0.4.0] - 2026-09-01

Pins `embassy-supervisor-syntax = "=0.3.0"`.

### Added

- `supervisor-lint --only public-gate`: reports a `Backed`, `Leased` or
  `VetoGate` static with any visibility but private, with file and line.
  `scan_gate_statics` / `GateStatic` / `gate_lints` are the library side;
  `inputs::Scan` carries `gate_statics`.
- `ResourceModel.divisible` / `.serialized` and `SignalModel.veto`, with the
  matching `model_json` keys; the runtime diagram labels the edges
  (`divisible`, `shared · serialized`, `veto`) and the state diagram's takes
  name the kind.

## [0.3.0] - 2026-08-30

### Added

- `model::full_model(&[Decl]) -> FullModel`: a typed, faithful projection of
  every clause the grammar carries — the fields `model_json` never mapped
  (`slot_timeout:`, `ack_timeout:`, `beat_timeout:`, `beat_window:`,
  `ready_on_write`, the `spawn:`/`task:` source, `pool_size:`, `exit:`,
  `state:`, `cancel`, and a pool's `min:`/`max:`/`policy:`/timeouts/`state:`)
  now come back as plain data, with literals and expressions handed over as
  parsed plus their token text. Interpretation stays with the consumer.
- `model_json` carries the same clauses as additive keys and is now a
  projection of `full_model`, so the two cannot drift. Existing keys are
  unchanged; readers that do not look for the new ones are unaffected.

## [0.2.0] - 2026-08-27

Baseline for this changelog.

[Unreleased]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.9.0...HEAD
[0.9.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.8.0...embassy-supervisor-tools-v0.9.0
[0.8.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.7.0...embassy-supervisor-tools-v0.8.0
[0.7.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.6.0...embassy-supervisor-tools-v0.7.0
[0.6.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.5.0...embassy-supervisor-tools-v0.6.0
[0.5.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.4.1...embassy-supervisor-tools-v0.5.0
[0.4.1]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.4.0...embassy-supervisor-tools-v0.4.1
[0.4.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.3.0...embassy-supervisor-tools-v0.4.0
[0.3.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.2.0...embassy-supervisor-tools-v0.3.0
[0.2.0]: https://github.com/cedrivard/embassy-supervisor/releases/tag/embassy-supervisor-tools-v0.2.0
