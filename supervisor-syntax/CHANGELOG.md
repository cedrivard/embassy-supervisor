# Changelog

All notable changes to `embassy-supervisor-syntax` are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

This crate is pinned by exact version from `embassy-supervisor-macros`: its AST is an
internal contract between the two, not a stable public API, and it changes whenever the
graph syntax does. Publish it before the macro crate that depends on it.

## [0.2.0] - 2026-08-27

The AST changes shape; `embassy-supervisor-macros = "=0.8.0"` is the matching
consumer.

### Added

- `Gated<K, V>`: a clause keyword and value together with the `#[cfg(...)]`
  attributes gating it. Every value-level clause takes a gate — `slot_timeout:`,
  `ack_timeout:`, `beat_timeout:`, `beat_window:`, `ready_on_write`, `disabled`
  and `discover` — as does a single `provides:` entry (`ProvideDecl`, mirroring
  the other per-entry lists). A gate on anything structural is a spanned error
  naming the alternatives; the pool variant names the pool's own gateable set
  (`slot_timeout:`, `ack_timeout:`, `discover`) and per-entry lists.
- Predicate pairing: a gated `beat_timeout:` requires the token-identical
  predicate on any `beat_window:` or `ready_on_write` riding on it — an active
  claim must not outlive the budget its gate compiles out.

### Changed

- **Breaking**: `CommonClauses::slot_timeout`/`ack_timeout`/`discover` and
  `NodeItem::beat_timeout`/`beat_window`/`ready_on_write`/`disabled` are
  `Option<Gated<..>>`; `NodeItem::provides` is `Vec<ProvideDecl>`.
- syn 3.

### Fixed

- Duplicate `beat_timeout:`, `beat_window:`, `disabled` and `ready_on_write`
  clauses are errors like their siblings instead of last-wins — two
  differently gated copies would have silently dropped the first predicate.
- A malformed gate (`#[cfg]`, `#[cfg = "..."]`) is a spanned parse error
  instead of a downstream proc-macro panic or a silently dropped attribute.

## [0.1.0] - 2026-08-25

Initial release: the `supervisor_graph!` grammar, extracted from
`embassy-supervisor-macros` so that more than one thing can read it. A `proc-macro`
crate cannot export anything but its macros, so the parser had been unreachable to the
tooling that wants it.

### Added

- `GraphSpec` and its `Parse` impl, covering `node`, `pool`, `executor` and `observe`
  items, the `name:` header, and every clause and marker the graph DSL accepts —
  `state:` carrying a `StateInit` (`Expr(init)` or `Zeroed(kw)`, with `zeroed` a
  contextual keyword) included.
- Shape checks that belong to the grammar rather than to any build's policy: empty
  clause lists, a repeated signal path, a path declared both bare and indexed, `bound`
  without `ready`, a zero `slot_timeout:` or `ack_timeout:`, duplicate clauses,
  the `beat_timeout:` / `beat_window:` ranges, the `discover`
  rules (a list beside it may only add markers, nothing parked), the `dataflow:` adoption
  list's shape (non-empty, no duplicates), the entry-marker rules
  (`via` needs `observed`, and `beat` only qualifies an `observed` write), and
  the `ready_on_write` shape (an `observed beat` entry in `writes:`, plus
  `beat_timeout:`).
- `substitute_dollar_crate`, which resolves the `$crate` a `supervisor_fragment!` writes
  to whatever the caller supplies, and `normalize_fragment_crate`, which rewrites a
  bare `crate` to `$crate` in **fragment** tokens — inside a fragment the two can only
  mean the fragment's own crate — so both spellings resolve alike there, while a plain
  graph's bare `crate` stays exactly what it says.
- The `#[dataflow]` scanner (`rewrite_verb_calls`, `node_param`,
  `scan_dataflow`, `Access`): one receiver-keyed walker over a fn's verb calls,
  shared by the attribute macro (which also rewrites the sites) and the diagram
  tool — so what the build derives and what the diagrams draw cannot drift.

### Notes

- **No features.** The grammar always parses in full, including constructs the
  supervisor gates (`ready`, `bound`, `observed`, `beat`, `discover`,
  `local`, `state:`, `beat_timeout:`, `ready_on_write`). Every one keeps the `Ident` or literal a rejection
  would point at, so a caller applies its own policy afterwards with real spans.
- **No name resolution.** Unknown deps, duplicate names and the 256-slot cap are checked
  against a whole graph by the macro. A fragment legitimately names nodes it does not
  contain, so parsing one in isolation has to succeed.
