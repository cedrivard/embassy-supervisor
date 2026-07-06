# Changelog

All notable changes to `embassy-supervisor` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-07-05

**Breaking release** — bring-up is now `async` (see *Changed*). Adds multi-executor
graphs, multi-core support, and trace-hook observability.

### Added
- Multi-executor graphs: the `executor NAME;` item declares a runtime-filled
  `SpawnerSlot` (a `SendSpawner` — `InterruptExecutor` tiers, the second core, or a
  foreign thread executor via `make_send()`), and `executor: NAME` on a node routes its
  generated spawn through the slot. An unfilled slot fails the spawn with
  `SpawnError::Busy`; annotated nodes' futures must be `Send`.
- `TaskNode::adopt(&SpawnToken)`: one-call registration (task id + `trace-names` name
  stamp) for spawns the macro cannot see (parked nodes, verbatim spawn closures).
- `trace-nested` (opt-in): preemption-exact accounting. A nested higher-tier poll
  credits its wall time back to the window it interrupted, so a preempted node's
  `exec_ticks`/`max_poll_ticks` are no longer inflated and `stalled_task`/watermarks
  name the real culprit. On multi-core systems register `trace::set_core_id_fn`
  (e.g. read `SIO.CPUID` on RP2350) for one preemption stack per core; unregistered,
  everything maps to core 0 (single-core behavior).
- Multi-core support: bring-up awaits a node's `SpawnerSlot` (`ready()`) so the
  supervisor rendezvouses with another core's asynchronous executor bring-up *as part
  of* `start()` (bounded, then `SpawnError::Busy`); pools accept `executor: NAME` too
  (an elastic worker pool on the second core, scaled by this core's supervisor).
  Cross-core lifecycle (slot-routed spawn, shutdown/ack, control) is covered by
  cross-thread host tests running two real executors.
- `TaskNode::with_executor(&SpawnerSlot)`: routes a node's spawn through an executor
  slot (emitted by `supervisor_graph!` for `executor: NAME`).
- `supervisor_graph!` `deps:` may name a `pool` (not only a `node`); it resolves to the
  pool's floor member — "start after the pool is up".
- Trace-hook observability (opt-in features): `trace` — the supervisor consumes
  embassy-executor's `_embassy_trace_*` instrumentation, mapping task ids to nodes via the
  generated spawn glue and accounting per-node poll time / poll count / max-poll watermark,
  per-executor idle time, and live stall detection (`trace::current_task` /
  `trace::stalled_task`), and a per-executor time decomposition (`trace::executor_stats`:
  idle / in-poll / overhead / unsupervised-task share, poll and pass counters);
  `trace-hooks` — `supervisor_graph!` also defines the hook symbols
  at the declaration site; `trace-names` — node names are stamped into task Metadata for
  external consumers. Counters are wrapping u32 ticks (sample-and-diff); accounting is
  preemption-naive and capped at 4 executors (documented).

### Changed
- **Breaking:** `Supervisor::start`, `Supervisor::start_node`, and
  `Supervisor::respawn_terminate` are now `async fn` and must be `.await`ed (they were
  synchronous). Bringing up an `executor: NAME` node now awaits its `SpawnerSlot`
  (bounded by an internal default, then `SpawnError::Busy`) before spawning it, so a
  tier filled late — or from another core — is handled without a race and without the
  hazards of the old synchronous slot wait (no busy-spin; no integrated-timer-queue
  panic on hardware). A node with no `executor:` slot skips the wait. Callers on the
  supervisor task simply add `.await`.

### Fixed
- `pool` without `control` (`default-features = false, features = ["pool"]`) failed to
  compile: the graph-index helpers the pool driver needs lived in a `control`-gated
  impl. They are now gated on either feature.
- Control `Activate` of a detached node no longer re-enables (and potentially
  restarts) the node's dependencies: a detached node's `deps:` are start-ordering
  only, so the activate cascade now skips expanding from a detached member, matching
  the deactivate cascade.
- `trace-nested`: an unpaired executor-end hook (its begin was skipped because the
  executor registered mid-poll) no longer underflows the per-core nesting depth,
  which permanently desynced preemption attribution on that core.

## [0.2.0] - 2026-07-01

The graph moved to compile time, and the `supervisor_graph!` proc-macro shipped in the
new companion crate `embassy-supervisor-macros` (versioned independently and pinned by
exact version; pulled in by the default `macros` feature). **Breaking release** — see
the migration notes in the README.

### Added
- Pool `policy:` accepts an optional explicit type: `policy: <Type> = <expr>`. When the
  type is omitted it is still derived from a `Type::new(..)` value (unchanged); the
  explicit form allows any value of that type (a `const`, a `const fn` factory, a builder
  chain, a qualified path).
- `Debug` impls on `Mode`, `ControlOp`, `ControlCommand`, and `TaskNode` (the latter a
  manual impl printing the name, mode, and live state flags).
- Macro-time validation: graphs are capped at 256 node slots (all graph indices are `u8`;
  a larger graph previously truncated silently), and pool bounds must satisfy
  `min <= max <= member count`.

### Changed
- The graph-declaration macro was renamed from `task_graph!` to `supervisor_graph!`.
- `supervisor_graph!` now emits a single `pub static GRAPH: Graph<M>` bundling the node
  slots, dependency table, topological order, and (with the `pool` feature) the pools,
  replacing the former loose `ALL_NODES` / `DEPS` / `ORDER` / `POOLS` symbols. Read them as
  `GRAPH.nodes` / `GRAPH.deps` / `GRAPH.order` / `GRAPH.pools`.
- `Supervisor::new` takes the bundled graph: `Supervisor::new(&GRAPH)`, replacing the
  previous three-argument `new(&ALL_NODES, &DEPS, ORDER)` form.
- `Supervisor::run_pools` no longer takes a pool-registry argument; it reads the pools from
  the graph (`GRAPH.pools`).

### Removed
- `Supervisor::with_pools` — pools are now part of `GRAPH` and passed via `Supervisor::new`.
- The generated `NODE_COUNT` constant; use `GRAPH.nodes.len()` instead.

### Internal
- Host-runnable unit + integration tests for the dependency-ordered topo sort, cycle
  detection, and the `DeferredShrink`/`ElasticPool` scaling logic, plus a GitHub Actions CI
  workflow (host tests, `thumbv8m` no_std build, clippy, fmt, doc). Test-only dev-dependencies
  are gated to non-embedded targets, so the shipped crate stays `no_std` and driver-agnostic.

## [0.1.1]

### Fixed
- `task_graph!` accepts the final node with or without a trailing comma. The documented
  `task_graph! { &A, &B }` form (no trailing comma) previously hit a macro recursion limit;
  both styles now expand correctly.

## [0.1.0]

Initial release.

- Dependency-ordered task bring-up and reverse-ordered teardown over a `task_graph!` of `TaskNode`s
  (topological sort, no allocation).
- Lifecycle modes: `Terminate`, `Pause`, `OnDemand`.
- Elastic worker pools (`ElasticPool` with a swappable `ScalingPolicy`, e.g. `DeferredShrink`)
  behind the `pool` feature.
- Decoupled runtime start/stop/pause/resume control (`request_control` / `apply_control`) behind the
  `control` feature.
- Optional `defmt` logging behind the `defmt` feature (no-op otherwise).

[0.3.0]: https://github.com/cedrivard/embassy-supervisor/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cedrivard/embassy-supervisor/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/cedrivard/embassy-supervisor/releases/tag/v0.1.1
[0.1.0]: https://github.com/cedrivard/embassy-supervisor/releases/tag/v0.1.0
