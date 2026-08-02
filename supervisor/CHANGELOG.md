# Changelog

All notable changes to `embassy-supervisor` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-08-02

Defect fixes: the control mailbox can no
longer drop a request silently, a task body that returns on its own is now observed,
and a missed shutdown ack is an error the application escalates instead of a panic
inside the supervisor. Macro pin moves to `embassy-supervisor-macros = "=0.5.0"`.

### Changed (breaking)
- `request_control` is now `async` and lossless: it awaits mailbox capacity instead
  of silently dropping when the 4-deep channel is full. The old fire-and-forget
  behavior is gone; for sync contexts (ISRs, callbacks) use the new
  `try_request_control`, which returns `Err(ControlQueueFull)` instead of dropping.
- The shutdown paths no longer panic on a missed ack. `stop_node`, `teardown` and
  `apply_control` now return `Result<(), ShutdownTimeout>` naming the offending
  node; `run_pools` returns `ShutdownTimeout` (it only completes on error, from a
  shrink whose member missed its ack). `teardown` aborts the cascade at the first
  timeout so a still-live dependent never has its dependencies stopped under it;
  the previous behavior is one token away (`.unwrap()` / `defmt::unwrap!`).

### Added
- `TaskNode::mark_exited()` / `TaskNode::has_exited()`: a task body that returns is
  now recorded as completed (and the teardown handshake acked). The generated
  `task:` shell calls `mark_exited()` automatically after the worker returns and
  resources are restored, so an autonomous exit no longer reads as running forever
  and a control `Activate` can respawn it. Hand-written `spawn:` tasks call
  `mark_exited()` where they previously called `ack_dropped()` on exit.
  `has_exited() && !shutdown_requested()` distinguishes an autonomous completion
  from an acked stop.
- `Supervisor::teardown_continue()`: best-effort teardown for the
  "hardware reset next" escalation — visits every node in reverse order past a
  non-acking one and returns the first `ShutdownTimeout` at the end.
- `ControlQueueFull` and `ShutdownTimeout` error types (`defmt::Format` under the
  `defmt` feature).
- **`readiness` feature** (off by default): task-asserted readiness. Providers call
  `set_ready()` once actually serving (DHCP bound, registration done);
  `deps: [NET ready]` then holds a dependent's spawn until the assertion (bounded by
  the dependent's `slot_timeout`, then `SpawnError::Busy` with a log line naming the
  not-ready dep — plain deps still order spawns only). Elastic-pool growth defers
  while a ready-marked dep is un-ready. `clear_ready()` withdraws readiness as
  status, not control (dependents are never stopped); the pre-spawn reset clears it
  so a respawned provider re-asserts. Costs one AtomicBool + one Signal + one slice
  per node.
- **`liveness` feature** (off by default): per-node heartbeat. Bodies call `beat()`
  per work loop; an app watchdog reads `is_stale(max_age)` to catch
  alive-but-wedged tasks (parked on an await that will never complete) — the
  complement of the `trace` stall watermark, and independent of `trace`.
  `set_running` stamps a beat so a fresh spawn is never instantly stale;
  not-running nodes are never stale. Costs one AtomicU32 per node.
- `TaskNode::run_cancellable(fut)` / `run_cancellable_acked(fut)`: the select
  against `wait_shutdown()` as a combinator — `Ok(output)` on completion,
  `Err(Aborted)` when a stop wins; the `_acked` variant completes the handshake
  before returning, for bodies with no cleanup between the select and the ack.
- **Named multi-graphs**: `name: IDENT;` as a graph's first item (and
  `compose_graph! { name: X, … }`) renames the emitted static and suffixes every
  generated helper, so several supervisors coexist per binary — the shape a
  subordinate sub-graph wants (a dedicated graph the app state machine cycles
  with `start()`/`teardown()` per phase). The unnamed graph is the primary: only
  it emits the `trace-hooks` symbols; the trace registry now tracks up to
  `trace::MAX_GRAPHS` graphs. The control mailbox is shared — run ONE driver and
  apply each command to every supervisor (foreign-node commands no-op safely).
- `Supervisor::resume_node(node)`: resume ONE `Pause` node parked by an earlier
  `stop_node`/`teardown` — the single-node partner of `resume_pausable` (same
  sequence, same deliberate absence of dependency gating), completing the
  node-level verb set: `stop_node` is the single-node pause for a `Pause` node,
  `resume_node` the other half. No-op unless the node is `Pause`, actually
  parked, and neither disabled nor detached.
- `Supervisor::activate(node, spawner)` / `Supervisor::deactivate(node)` are now
  public — the cascading, `disabled`-latching subsystem verbs, previously
  reachable only by wrapping a `ControlCommand` for `apply_control` even when
  holding the supervisor directly. `apply_control` remains the mailbox-dispatch
  form. `activate` returns `()` (cascade spawn errors are deliberately
  swallowed and re-driven); `deactivate` returns `Result<(), ShutdownTimeout>`.
- `Supervisor::run(spawner) -> RunError`: the canonical driver as one call —
  `start()` then drive pool scaling and runtime control forever, returning only
  on error (`RunError::{Spawn, Shutdown}`). The manual
  `select(run_pools, wait_control)` loop remains for apps with extra wake
  sources.
- `start()` is now the universal quiescent-to-running op, so
  `start()`/`teardown()` cycles on a (sub-graph) supervisor handle every mode:
  each node is reset before its spawn (as `start_node` always did), running and
  detached nodes are skipped (idempotent; a detached instance survived the
  teardown), and a `Pause` instance parked by an earlier teardown is resumed in
  place instead of double-spawned — the completion flag (`mark_exited`) is what
  makes "parked" (`acked && !completed`) distinguishable from "exited". The
  resume path bypasses the gate waits like `resume_pausable` (a parked instance
  retains its resources, so its slots are empty by design).
- **`heap-state` feature** (off by default): the `state: Type = init_expr` clause
  on `task:` nodes and pool members — reclaimable per-activation heap state. The
  spawn glue fallibly boxes the init value (alloc failure = `SpawnError::Busy`,
  nothing spawned or stranded, retryable), the shell lends the worker `&mut Type`
  and drops the Box on task exit, before restores and the completion record —
  every activation allocates fresh, every exit frees, net zero across respawns.
  Task STORAGE stays static by design: embassy wakers are unrefcounted pointers
  into it, so freeing it is unsound — heap only where it can be
  reclaimed. The runtime crate stays no-alloc/`forbid(unsafe_code)`; the ~6-line
  fallible-boxing helper (the feature's entire unsafe surface) is emitted into
  the consumer crate, like `local-resources`. Consumer needs a
  `#[global_allocator]`.
- **Composable graphs**: `supervisor_fragment! { name: X; <items> }` lets a module
  or a whole crate declare its slice of the graph; `compose_graph! { fragments:
  [X, ::other::Y], graph: { .. } }` assembles them into ONE `supervisor_graph!`
  expansion — cross-fragment deps resolve by name in either direction, and every
  compile-time pass (name map, u8 slots, topo order, shared-slot dedup, 256 cap)
  checks the whole composed graph. Errors are attributed to the owning fragment.
  Fragment paths use `$crate::…`; `#[cfg]` inside a fragment evaluates against the
  compose crate's features (documented).
- **Per-member pool resources**: pools now accept take-kind `resources:`
  entries (default lend and `consume`), emitted as per-member slot arrays
  `[ResourceSlot<T>; K]` — member `I` takes/restores element `I` exclusively, the
  floor comes up with floor-many elements provided, and a lend value survives
  shrink/regrow on the same index. `shared` stays pool-wide — `shared local`
  included, as in 0.3.x; only take-kind `local` on pools stays rejected (the
  single-core slot contract + per-member restore is deferred). New
  `ElasticPool::member_index(node)` lets a worker index per-member app state.
- **Const-expression pool bounds**: `min:`/`max:` accept any const-evaluable
  `usize` expression; non-literals make the emitted `_MIN`/`_MAX` consts the source
  of truth, guarded by const asserts (min <= max <= members <= 255). The member
  count (the mode list) stays a literal: it drives how many items are emitted,
  which a proc macro cannot derive from a const.
- `exit: Type` graph clause (`task:` nodes): the worker's return value is
  `provide()`d into a generated `pub static <NODE>_EXIT: ResourceSlot<Type>` just
  before the completion is recorded, so `has_exited()` implies the value is
  readable. `ResourceSlot::wait_take()` awaits and takes it. Idiom: a worker
  returning `Result<R, Aborted>` straight out of `run_cancellable` records
  completed-vs-cancelled.

## [0.3.5] - 2026-07-27

RAM saving for the `Supervisor` itself: the topological order is now borrowed from
the `static` graph instead of copied into the struct. No API, feature, or behavior
change; the macro pin is unchanged (`embassy-supervisor-macros = "=0.4.1"`).

### Changed
- `Supervisor<N>` holds `&'static [u8; N]` for the precomputed order rather than an
  inline `[u8; N]`. A supervisor usually lives inside a task future — i.e. in that
  task's `static` storage — so the copy cost N bytes of RAM per supervisor, plus the
  code to make it, for no benefit: `Supervisor::new` already takes `&'static Graph<N>`,
  so the array it copied from outlives the supervisor by construction. Net saving is
  `N - 4` bytes on 32-bit targets. The field is private and `new` is unchanged, so this
  is source-compatible.

## [0.3.4] - 2026-07-09

Safety fix to 0.3.3 (macro pin -> `embassy-supervisor-macros = "=0.4.1"`): using the
`local` resource kind now requires the new **`local-resources`** feature, OFF by
default. `local` is the one graph form that makes `supervisor_graph!` emit `unsafe`
code into the consuming crate (the local slot type's `unsafe impl Sync`, whose
soundness contract — every `provide`/`take`/`restore` of a slot on ONE core — the
application owns), so it is now an explicit per-application opt-in. Graphs not using
`local` are unaffected; `local` users add the feature and change nothing else.

### Added
- The `local-resources` feature (non-default): permits the `local` resource kind by
  forwarding to the macro crate. Without it, a `local` marker is a compile error
  naming the feature.

## [0.3.3] - 2026-07-09

Ships the `resources:` **kind markers** by updating the macro pin to
`embassy-supervisor-macros = "=0.4.0"`: `consume` (the worker owns the value — drop it
at teardown; the slot stays empty, so a respawn fail-closes until the app re-provides a
fresh instance), `shared` (a fan-out slot for a `Copy` handle — any number of nodes and
whole pools copy the same value out non-destructively; replaces panicking accessor
extras with a gate-awaited `SpawnError::Busy`), and `local` (a graph-site slot without
the `T: Send` bound, for `!Send` driver handles on a single core). Composed
(`local consume`, `shared local`) they cover the whole radio shape: a `!Send` runner
whose `Drop` must release pins/DMA at teardown and that is rebuilt each wake cycle,
plus the `Stack` handle fanned out to every network consumer. With the new
`slot_timeout:` clause, the builder itself becomes a **provider node** — an ordinary
first-in-topo node that `provide()`s at runtime while its consumers' gate waits
rendezvous with the build (the graph-native `hw_init`; see the README's provider-node
recipe). See the macros CHANGELOG and the README's "Resource kinds" section.

### Added
- `ResourceSlot::get` (`T: Copy` only): copy the value out **without emptying the
  slot** — the `shared` kind's fan-out read, also usable by hand.
- `TaskNode::with_slot_timeout` (backs the macro's `slot_timeout:` clause): per-node
  override of the pre-spawn `executor:`-slot and `resources:`-gate wait bound (the
  default is unchanged at 100 ms). Size it to a provider node's async build time.

## [0.3.2] - 2026-07-08

Decouples node-name stamping from the trace recorders, so a graph's node names can
reach external consumers (rtos-trace/SystemView, debuggers) **without** the
supervisor's own `trace` layer or its `_embassy_trace_*` hook symbols. Updates the
macro pin to `embassy-supervisor-macros = "=0.3.1"`.

### Added
- `TaskNode::stamp_name` (feature `metadata-names`): stamps the node name into the
  task's embassy `Metadata` without capturing the task id or engaging the `trace`
  recorders. Called automatically by the generated spawn glue on the name-only path.
- The `metadata-names` feature: pulls only `embassy-executor/metadata-name` (not
  `embassy-executor/trace`), so a binary built with `metadata-names` (and no
  `trace-hooks`) links with zero `_embassy_trace_*` symbols and pairs cleanly with
  embassy's own `rtos-trace` feature — SystemView shows the graph node names at no
  supervisor-recorder cost.

### Changed
- `trace-names` is now `["trace", "metadata-names"]` (previously it enabled
  `embassy-executor/metadata-name` directly). The activation set and effective
  behavior are identical to prior releases; the name stamp is simply now a
  standalone capability that `trace-names` composes.

## [0.3.1] - 2026-07-07

Ships the `supervisor_graph!` `task:` clause by updating the macro pin to
`embassy-supervisor-macros = "=0.3.0"`: declare a **plain async worker fn** —
possibly generic — and the macro stamps its concrete `#[embassy_executor::task]`
shell per node. See the macros CHANGELOG and the README's "`task:` —
generated shells" section.

### Added
- **Safe resource threading** (backs the macro's new `resources:` clause):
  `ResourceSlot<T>` — a one-value handoff cell moving an owned resource from `main`
  into a supervised task, replacing `Peripherals::steal()` inside the task body.
  `main` `provide()`s the value (consuming the `Peripherals` field — the
  compile-time exclusive-ownership guarantee), the generated spawn glue `take()`s it
  before the spawn (empty slot → `SpawnError::Busy` out of `start()`, fail-closed),
  and the generated shell `restore()`s it after the worker returns so a Terminate
  respawn re-takes the *same instance*. Provisioning is runtime-checked; ownership
  is compile-time.
- `ResourceGate` (the slot's type-erased readiness view) and
  `TaskNode::with_resources`: `start` / `start_node` / `respawn_terminate` await a
  node's resource slots being filled (bounded by the same `SLOT_READY_TIMEOUT` as
  executor slots) before spawning — tolerating late provisioning and closing the
  respawn-races-the-restore window on multi-core graphs.

### Changed
- `defmt` dependency requirement stated as `1.1.0` (the version the crate is built
  and tested against) instead of the imprecise `1`.

## [0.3.0] - 2026-07-06

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

[0.4.0]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.5...v0.4.0
[0.3.5]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/cedrivard/embassy-supervisor/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cedrivard/embassy-supervisor/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/cedrivard/embassy-supervisor/releases/tag/v0.1.1
[0.1.0]: https://github.com/cedrivard/embassy-supervisor/releases/tag/v0.1.0
