# Changelog

All notable changes to `embassy-supervisor-macros` are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The crate is versioned
independently of `embassy-supervisor`, which pins it by exact version; see the
supervisor's CHANGELOG for the surrounding API history.

## [0.2.0] - 2026-07-06

Requires `embassy-supervisor` >= 0.3.0 (the generated `executor:` glue uses that
release's async slot rendezvous); pinned by exact version from the supervisor crate.

### Added
- The `executor NAME;` item (emits a `pub static NAME: SpawnerSlot`) and the
  `executor: NAME` node clause: the generated glue spawns through the named slot's
  `SendSpawner` instead of the supervisor's `Spawner`. Unknown names, `executor:`
  without `spawn:`, and `executor:` with a verbatim closure are expansion errors.
- Pools accept `executor: NAME` too (between `deps:` and `spawn:`): every member
  spawns through the slot — a worker pool on another executor or core.
- `deps:` may name a `pool` (not just a `node`); the dep resolves to the pool's floor
  member (member 0, the `min`-kept one), i.e. "start after the pool is up". Previously a
  dep on a pool name was an "unknown dependency" error.
- A repeated dependency (`deps: [A, A]`; compared by resolved slot, so a repeated pool
  name counts too) and a redeclared node/pool name are now spanned compile errors.
  Previously a duplicate dep surfaced as a bogus "dependency cycle" and a duplicate
  name silently rewired earlier `deps:` edges before failing downstream.
- Pool `min:`/`max:` emit the validated `u8` values instead of the raw literals, so a
  suffixed literal (`min: 3usize`) no longer produces a mismatched-type rustc error.
- The unknown-dependency error now says "not a declared node or pool".
- An `executor:` node/pool now emits `TaskNode::with_executor(&NAME)`; its spawn glue
  does a non-blocking `SpawnerSlot::get()` because the supervisor awaits the slot
  before invoking it (see the supervisor's 0.3.0 async bring-up).
- Forwarded trace features: under `trace` the generated spawn glue captures each
  `SpawnToken`'s task id into its node (`set_task_id`); under `trace-names` it also stamps
  the node name into the task Metadata; under `trace-hooks` the macro defines the seven
  `_embassy_trace_*` hook symbols at the graph declaration site (the supervisor crate is
  `forbid(unsafe_code)` and cannot; requires an edition-2024 consumer).

## [0.1.0] - 2026-07-01

First published version (previously an unpublished workspace member).

- `supervisor_graph!`: declares `node`/`pool` items once and emits the node `static`s,
  per-pool `ElasticPool` + spawn glue, and a single `pub static GRAPH: Graph<M>` bundling
  the node slots, dependency table, elastic pools, and the topological order computed at
  compile time (a dependency cycle or unknown dependency is a compile error).
- Items and individual deps may carry `#[cfg(...)]`; absent nodes keep their slot as `None`.
- Pool `policy:` accepts an optional explicit type (`policy: <Type> = <expr>`); without it
  the type is derived from a `Type::new(..)`-shaped value.
- Graphs are capped at 256 node slots (indices are `u8`); pool bounds are validated
  (`min <= max <= member count`) at expansion time.
- The `pool` feature (forwarded by `embassy-supervisor`) gates pool emission.

[0.2.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.1.0...embassy-supervisor-macros-v0.2.0
[0.1.0]: https://github.com/cedrivard/embassy-supervisor/releases/tag/embassy-supervisor-macros-v0.1.0
