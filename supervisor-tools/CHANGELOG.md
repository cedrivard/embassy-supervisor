# Changelog

All notable changes to `embassy-supervisor-tools` are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
