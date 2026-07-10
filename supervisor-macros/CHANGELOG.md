# Changelog

All notable changes to `embassy-supervisor-macros` are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The crate is versioned
independently of `embassy-supervisor`, which pins it by exact version; see the
supervisor's CHANGELOG for the surrounding API history.

## [0.4.0] - 2026-07-09

Requires `embassy-supervisor` >= 0.3.3 (the generated `local` slot type names its
`_export` shim); pinned by exact version from the supervisor crate (`=0.4.0` as of
supervisor 0.3.3).

### Added
- `resources:` **kind markers** — per-entry, order-free, composable:
  `resources: [NAME: [local] [shared|consume] Type, ..]`.
  - `consume`: the worker receives the value **by value** and the shell emits no
    restore — the slot stays empty after the task exits, so the worker may *drop* the
    resource at teardown (a driver whose `Drop` releases pins/DMA) and a respawn
    fail-closes with `SpawnError::Busy` until the application `provide()`s a fresh
    value (the pattern for resources rebuilt each wake cycle).
  - `local`: the entry's slot is a graph-site type (`__SvLocalResourceSlot`) with the
    `ResourceSlot` protocol but no `T: Send` bound, for `!Send` driver handles
    (`RefCell`-/`NoopRawMutex`-based) on a single core. Emitted at the declaration
    site because it carries an `unsafe impl Sync`; its soundness contract is
    single-core use, and a consumer crate forbidding `unsafe_code` cannot use it.
  - `shared`: a fan-out slot for a `Copy` handle — the glue copies the value out
    non-destructively (`get()`, whose `T: Copy` bound enforces the kind), the worker
    receives it by value, no restore, and the slot STAYS FILLED. Any number of nodes
    (and whole `task:` pools — the only `resources:` kind pools accept) may declare
    the SAME slot name: the static is emitted once, gated by the union of the
    declaring sites' `#[cfg]` predicates, and every re-declaration must repeat the
    kinds + type verbatim. Mutually exclusive with `consume`.
  - The markers are contextual keywords: `local`/`consume`/`shared` followed by `::`,
    `<`, or the entry end still parse as (part of) the type.
- `resources:` entries accept per-entry `#[cfg(...)]`: the slot static, gate entry,
  glue take/get, shell parameter, and worker-call argument all follow it (gate the
  worker fn's matching parameter with the same attribute). The node's gate array
  length is cfg-aware.
- The `slot_timeout: MS` clause (nodes and pools; milliseconds ≥ 1) — emits
  `TaskNode::with_slot_timeout`, overriding the 100 ms default bound on the pre-spawn
  `executor:`-slot and `resources:`-gate waits. Sized to a **provider node**'s async
  build time, it turns runtime provisioning into a rendezvous (see the README's
  provider-node recipe).
- New compile errors: a repeated kind marker on one entry; `shared` combined with
  `consume`; a `shared` slot re-declared with different kinds/type; a non-`shared`
  resource on a `pool` (previously all pool `resources:` were rejected); pool
  `resources:` without `task:`; `slot_timeout: 0`; and `local` combined with
  `executor:` on a node or a pool (a `SpawnerSlot`-routed spawn needs a `Send`
  future).
- Generated shells with restore statements carry `#[allow(unreachable_code)]`, so a
  diverging (`-> !`) worker with restore-kind resources no longer warns on the
  (legitimately) unreachable restores.

## [0.3.1] - 2026-07-08

Requires `embassy-supervisor` >= 0.3.2; pinned by exact version from the supervisor
crate (`=0.3.1` as of supervisor 0.3.2).

### Added
- The `metadata-names` feature: a name-only spawn path. When it is on but `trace` is
  off, `spawn_stmts` binds the `SpawnToken` and calls `TaskNode::stamp_name(&token)`
  (node name → task `Metadata`) instead of `adopt` — no task-id capture and no
  dependency on the `_embassy_trace_*` hooks, so a graph gets its node names into
  external consumers (rtos-trace/SystemView) without pulling in the trace recorders.

### Changed
- `trace-names` is redefined as `["trace", "metadata-names"]` (was `["trace"]`). Same
  effective codegen when `trace` is on (the `adopt` path, which stamps the name under
  `metadata-names`); the split just lets the name stamp be requested on its own.

## [0.3.0] - 2026-07-07

Requires `embassy-supervisor` >= 0.3.0; pinned by exact version from the supervisor
crate (`=0.3.0` as of supervisor 0.3.1).

### Added
- The `task:` node/pool clause: name a **plain async worker fn** — possibly generic —
  and the macro stamps the concrete `#[embassy_executor::task]` shell per declaration
  (embassy forbids generic tasks: one static `TaskPool` per concrete future type).
  Same path/partial-call forms as `spawn:`; worker args are evaluated inside the shell
  at the task's first poll, on the node's own executor; trace adoption and `executor:`
  routing compose unchanged.
- `pool_size: N` on a `task:` node sizes the generated shell's `TaskPool` (default 1);
  a `task:` pool emits one shell sized to the member count.
- The `resources: [NAME: Type, ..]` node clause (requires `task:`) — **safe resource
  threading**: each entry emits a `pub static NAME: ResourceSlot<Type>` at the
  declaration site. `main` moves the resource in with `NAME.provide(..)` (consuming
  the `Peripherals` field — compile-time exclusive ownership, no `steal()` inside the
  task), the generated glue `take()`s it just before the spawn (an unprovided slot
  fails `Supervisor::start` with `SpawnError::Busy`, not a task-side panic), and the
  shell hands the worker `&mut Type` (after the node arg, in declared order, before
  partial-call extras) and `restore()`s the value after the worker returns — a
  Terminate respawn re-takes the *same instance*. The node is emitted with
  `.with_resources(..)` so the supervisor awaits provisioning/restore before each
  (re)spawn.
- Each `pool` also emits the structural `pub const`s `<POOL>_MIN`, `<POOL>_MAX`, and
  `<POOL>_MEMBERS` (`usize`), so downstream compile-time sizing can derive from the
  DSL instead of duplicating it (e.g. `const SOCKET_BUDGET: usize = HTTP_MAX + 1`) —
  a `const` cannot read these off the member `static` array (E0013).
- New compile errors: `task:` combined with `spawn:`, a closure in `task:`,
  `pool_size:` without `task:` (or zero), `resources:` without `task:`, an empty
  `resources:` list, a duplicate resource name (within a node or across the graph),
  and `resources:` on a `pool` (members would contend for a single instance).

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

[0.4.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.3.1...embassy-supervisor-macros-v0.4.0
[0.3.1]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.3.0...embassy-supervisor-macros-v0.3.1
[0.3.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.2.0...embassy-supervisor-macros-v0.3.0
[0.2.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.1.0...embassy-supervisor-macros-v0.2.0
[0.1.0]: https://github.com/cedrivard/embassy-supervisor/releases/tag/embassy-supervisor-macros-v0.1.0
