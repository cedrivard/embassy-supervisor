# embassy-supervisor

[![crates.io](https://img.shields.io/crates/v/embassy-supervisor.svg)](https://crates.io/crates/embassy-supervisor)
[![docs.rs](https://docs.rs/embassy-supervisor/badge.svg)](https://docs.rs/embassy-supervisor)

A generic, **HAL-agnostic** task-lifecycle supervisor for the [embassy](https://embassy.dev)
async embedded framework. `no_std`, no allocator, no board crates — it compiles for any embassy
target. The only third-party deps are pure-embassy crates (`embassy-executor`/`-sync`/`-time`/
`-futures`) and `portable-atomic`.

## New in 0.2.0

- **Compile-time topology.** The graph is declared once with the new `supervisor_graph!`
  proc-macro (crate `embassy-supervisor-macros`, pulled in by the default `macros` feature).
  The topological order is computed at **compile time**: a dependency cycle or an unknown
  dependency is a *compile error*, and `Supervisor::new` is infallible and `const`.
- **One `GRAPH` bundle.** The macro emits a single `pub static GRAPH: Graph<N>` (node slots,
  dependency table, order, and pools) consumed by `Supervisor::new(&GRAPH)`.
- **Explicit pool policy type.** `policy: <Type> = <expr>` lets a pool's scaling policy be
  built by anything (a `const fn` factory, a builder chain); the shorthand
  `policy: Type::new(..)` still derives the type.
- **Stricter declarations.** Graphs are capped at 256 node slots (indices are `u8`), and pool
  bounds must satisfy `min <= max <= member count` — both checked at macro expansion.

Migrating from 0.1.x:

| 0.1.x | 0.2.0 |
|---|---|
| `task_graph! { &A, &B }` | `supervisor_graph! { node A = ...; node B = ...; }` |
| `Supervisor::new(&ALL_NODES, &DEPS, ORDER)` | `Supervisor::new(&GRAPH)` |
| `.with_pools(POOLS)` | gone — pools ride in `GRAPH` |
| `NODE_COUNT` | `GRAPH.nodes.len()` |

## What it does

- **Dependency-ordered lifecycle** — the supervisor brings tasks up in dependency order and
  tears dependents down before the things they depend on.
- **Lifecycle modes** — `Terminate` (started at boot, restartable), `Pause` (park/resume while
  keeping a held resource), `OnDemand` (started on demand to scale a pool).
- **Elastic pools** *(feature `pool`)* — `ElasticPool` scales a set of single-instance worker
  nodes with load via a swappable `ScalingPolicy` (e.g. `DeferredShrink`), within a fixed budget.
- **Runtime control** *(feature `control`)* — drive start/stop/pause/resume from anywhere (an HTTP
  endpoint, a button, …) through a decoupled mailbox (`request_control` / `apply_control`) that
  honors dependencies and pool membership.

The supervisor deliberately does **not** allocate, own a HAL, manage power states, or know what your
tasks do — it orchestrates their *lifecycle* and leaves the rest to you.

## Quickstart

```rust,ignore
use embassy_executor::Spawner;
use embassy_supervisor::{supervisor_graph, Supervisor, wait_control};

// Declare the graph once: `supervisor_graph!` generates the node `static`s and a
// single `GRAPH` bundling the node slots, dep table, compile-time order, and pools.
// Each `spawn:` names a task fn that is `s.spawn`ed with the node; `app` depends on `net`.
supervisor_graph! {
    node NET = Terminate, deps: [], spawn: net_task;
    node APP = Terminate, deps: [NET], spawn: app_task;
}

#[embassy_executor::task]
async fn supervisor_task(spawner: Spawner) {
    // Infallible: the order is precomputed, so a dependency cycle is a compile error.
    let sup = Supervisor::new(&GRAPH);
    sup.start(spawner).expect("initial spawn");   // brings up `net`, then `app`
    loop {
        let cmd = wait_control().await;           // runtime control requests
        sup.apply_control(cmd, spawner).await;    // applied in dependency order
    }
}
```

## The model

Three pieces, all `static`:

- **`TaskNode`** — one per managed task: a name, a [`Mode`], an optional spawn fn, and a
  private handle of atomic flags + signals. The *task side* of the protocol is a handful of
  node methods: a task selects its work against `wait_shutdown()`, calls `ack_dropped()` when
  it exits (or before parking), and — for `Pause` nodes — parks on `wait_resume()`. Pool
  workers additionally report load with `mark_busy()` / `mark_idle()` + `request_scale()`.
- **`Graph<N>`** — the macro-emitted `GRAPH`: `nodes` (fixed `[Option<&TaskNode>; N]` — a
  `#[cfg]`-ed-out node keeps its slot as `None`), `deps` (per-node dependency indices),
  `order` (the compile-time topological order), and `pools` (with the `pool` feature). The
  fields are public: a status endpoint can iterate them directly.
- **`Supervisor<N>`** — construction-free orchestration over `&GRAPH`: `start` /
  `teardown` / `resume_pausable` / `respawn_terminate` for whole-graph transitions,
  `start_node` / `stop_node` for single nodes, `apply_control` and `run_pools` as the driver
  loop's two engines.

`Mode` decides what each transition does to a node:

| mode | at boot | on teardown | on bring-up |
|---|---|---|---|
| `Terminate` | spawned | exits its loop (acks) | **respawned** (`respawn_terminate`) |
| `Pause` | spawned (or app-spawned if parked) | acks, then parks on `wait_resume()` | **resumed in place** (`resume_pausable`) — keeps held resources |
| `OnDemand` | not started | stopped like `Terminate` | not auto-started — pools/control start it |

A task that never acks a shutdown within the timeout panics the supervisor with the node's
name — a loud bug report, not a hang.

## The `supervisor_graph!` DSL

```text
node NAME = Mode, deps: [A, B], spawn: <spawn>[, disabled];
node NAME = Mode, deps: [A];      // no `spawn:` => parked node the app spawns itself
pool NAME = [Mode, ..], deps: [A],
    spawn: <fn>,
    policy: [<Type> =] <expr>,
    min: N, max: M;
```

- **`spawn:` forms** — a bare path `f` spawns `f(&NAME)`; a partial call `f(a, b)` spawns
  `f(&NAME, a, b)` (the node is always injected first); a closure is emitted verbatim (nodes
  only). Omit `spawn:` for a **parked** node whose task the application spawns itself (e.g. a
  `Pause` sensor holding a peripheral handle) — the supervisor tracks it but never spawns it.
- **`disabled`** — declared but not started at boot; a control `Activate` starts it later
  (e.g. an OTA task).
- **`#[cfg(...)]`** — on any `node`/`pool` *and on individual deps*. Absent nodes keep their
  slot as `None` and are skipped everywhere at runtime.
- **`pool`** — the mode list declares the members (floor first: typically
  `[Terminate, OnDemand, ...]`). The macro generates the member array `NAME: [TaskNode; K]`,
  per-member spawn glue, and a `NAME_POOL: ElasticPool<P>`. `policy:` takes the scaling
  policy; annotate the type explicitly (`policy: DeferredShrink = make_policy()`) when the
  value isn't a `Type::new(..)` constructor.
- **Limits** — at most 256 slots per graph; `min <= max <= K`. Violations are compile errors.

Generated surface at the call site: one `pub static` per node, the pool array + `NAME_POOL`,
and `pub static GRAPH` — nothing else.

## Runtime control

The `Supervisor` lives on the driver task's stack, so control is decoupled through a small
channel: any context calls `request_control(&NODE, ControlOp::Activate | Deactivate)` and
returns immediately; the driver loop receives it via `wait_control()` and runs
`apply_control`, which:

- **Deactivate** — tears down the node *and its transitive dependents*, dependents first.
- **Activate** — brings up the node's transitive deps first, then the node (respawn for
  `Terminate`, resume for `Pause`); a `disabled` node becomes enabled.
- **Pools** — control targeting any pool member is applied to the whole pool atomically.
- **Detached nodes** — a node that calls `set_detached(true)` manages its own lifecycle and
  is skipped by dependency cascades (see the OTA pattern below).

## Elastic pools

`ElasticPool` scales single-instance members between `min` and `max` running instances.
Workers report load (`mark_busy`/`mark_idle` + `request_scale`); the supervisor's
`run_pools(spawner)` future — `select`ed against `wait_control()` in the driver loop — wakes
on each scale request (it never polls), asks each pool's `ScalingPolicy` for a `PoolAction`,
and starts/stops one member accordingly.

The built-in `DeferredShrink` policy grows immediately when saturated (no idle member, below
`max`) and shrinks only after an idle surplus has persisted for a configurable cooldown —
responsive up, lazy down. Swap in your own policy by implementing `ScalingPolicy` (a sync,
allocation-free decision fn).

## Patterns

Recipes from the two real applications built on this crate (the in-repo `firmware`, and a
battery-powered sensor node):

- **Boot ordering** — declare `deps:` and call `start()`; done. `net` before `http`, `wifi`
  before everything.
- **Deep-sleep cycle** — before sleeping: `teardown().await` (reverse dependency order,
  every task acks). After waking: `resume_pausable()` for the parked sensors, then
  `respawn_terminate(spawner)` for the stateless services.
- **Connection worker pool** — floor of one `Terminate` listener + `OnDemand` spares,
  `DeferredShrink` policy: burst traffic grows the pool within ~one request, idle shrinks it
  after the cooldown, and `deps: [NET]` guarantees no worker outlives the network.
- **Control-started OTA** — declare the node `disabled`; an HTTP `POST /api/ota` calls
  `request_control(&OTA, Activate)`. The OTA task `set_detached(true)`s itself before
  draining the worker pool, so stopping its `NET` sibling-dependents doesn't cascade into it.
- **Status endpoint** — iterate `GRAPH.nodes` (name, `is_running()`, `is_busy()`,
  `is_disabled()`) and `GRAPH.deps` to render a live task table; the in-repo firmware serves
  exactly that as JSON + a dashboard.

## Cargo features

| feature   | default | what it adds |
|-----------|:-------:|--------------|
| `control` |    ✓    | runtime control plane (`ControlOp`, `request_control`, `apply_control`) |
| `pool`    |    ✓    | elastic worker pools (`ElasticPool`, `run_pools`, `GRAPH.pools`) |
| `macros`  |    ✓    | the `supervisor_graph!` graph-declaration macro |
| `defmt`   |         | route the supervisor's logs through `defmt` (otherwise the log macros are no-ops) |

`default-features = false` gives a minimal core that only does dependency-ordered
bring-up/teardown — dropping the control plane and pools trims flash and a couple of statics.

## `no_std` / MSRV

`#![no_std]` and `#![forbid(unsafe_code)]`. Requires Rust 1.85+ (edition 2024). The embassy
dependencies are pre-1.0 (`embassy-executor` 0.10, `embassy-sync` 0.8, `embassy-time` 0.5), so a
consuming application must use compatible embassy minor versions.

## Full example

The [`firmware`](https://github.com/cedrivard/embassy-supervisor/tree/main/firmware) crate in the
repository is a complete working application on an RP2350 — USB-CDC-NCM networking, an HTTP control
plane, an elastic worker pool, and OTA firmware update — all driven by this supervisor.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
