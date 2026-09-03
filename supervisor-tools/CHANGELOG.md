# Changelog

All notable changes to `embassy-supervisor-tools` are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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

[0.4.1]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.4.0...embassy-supervisor-tools-v0.4.1
[0.4.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.3.0...embassy-supervisor-tools-v0.4.0
[0.3.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-tools-v0.2.0...embassy-supervisor-tools-v0.3.0
[0.2.0]: https://github.com/cedrivard/embassy-supervisor/releases/tag/embassy-supervisor-tools-v0.2.0
