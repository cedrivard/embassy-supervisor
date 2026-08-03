# embassy-supervisor

[![crates.io](https://img.shields.io/crates/v/embassy-supervisor.svg)](https://crates.io/crates/embassy-supervisor)
[![docs.rs](https://docs.rs/embassy-supervisor/badge.svg)](https://docs.rs/embassy-supervisor)

A generic, **HAL-agnostic** task-lifecycle supervisor for the [embassy](https://embassy.dev)
async embedded framework. `no_std`, no allocator, no board crates — it compiles for any embassy
target. The only third-party deps are pure-embassy crates (`embassy-executor`/`-sync`/`-time`/
`-futures`) and `portable-atomic`.

## Table of contents

- [What it is](#what-it-is)
- [The grammar, at a glance](#the-grammar-at-a-glance)
- [Quickstart](#quickstart)
- [The model](#the-model)
- [Lifecycle reference](#lifecycle-reference)
- [Writing supervised tasks (the TaskNode API)](#writing-supervised-tasks-the-tasknode-api)
- [The `supervisor_graph!` DSL](#the-supervisor_graph-dsl)
- [Recipes by use case](#recipes-by-use-case)
- [Elastic pools](#elastic-pools)
- [Multi-executor tiers and multi-core](#multi-executor-tiers-and-multi-core)
- [Composing graphs across crates](#composing-graphs-across-crates)
- [Observability](#observability)
- [Cargo features](#cargo-features)
- [Testing on the host](#testing-on-the-host)
- [no_std / MSRV](#no_std--msrv)
- [Full example](#full-example)
- [Earlier release highlights](#earlier-release-highlights)
- [Migration](#migration)
- [License](#license)

## What it is

- **Dependency-ordered lifecycle** — the supervisor brings tasks up in dependency order and
  tears dependents down before the things they depend on.
- **Lifecycle modes** — `Terminate` (started at boot, restartable), `Pause` (park/resume while
  keeping a held resource), `OnDemand` (started on demand to scale a pool).
- **Elastic pools** *(feature `pool`)* — `ElasticPool` scales a set of single-instance worker
  nodes with load via a swappable `ScalingPolicy` (e.g. `DeferredShrink`), within a fixed budget.
- **Runtime control** *(feature `control`)* — drive start/stop/pause/resume from anywhere (an HTTP
  endpoint, a button, …) through a decoupled, **lossless** mailbox (`request_control` awaits
  capacity; the sync `try_request_control` reports a full queue instead of dropping) and a
  dependency- and pool-aware `apply_control`.
- **Whole-graph lifecycle ops** — `teardown()` / `teardown_continue()` (reverse-dependency
  shutdown with acked handshakes), `respawn_terminate()` (dependency-ordered re-spawn after a
  wake), `resume_pausable()` (thaw parked nodes); a missed shutdown ack comes back as a
  `ShutdownTimeout` error naming the node, never a hang or a library panic.
- **Multi-executor placement** — `executor:` annotations route nodes onto interrupt-priority
  tiers; the graph is the single source of *where each task runs*.
- **Multi-core placement.** The same mechanism spans the second core: `start()` rendezvouses
  with the other core's asynchronous executor bring-up as part of the bring-up loop, and a whole
  elastic pool can live on core 1, scaled by core 0's supervisor.
- **Safe resource threading** — `resources:` annotations move owned peripherals from `main`
  into workers through `ResourceSlot`s (compile-time exclusive ownership — no `steal()`),
  restored on task exit so a respawn re-takes the same instance.
- **Observability** *(feature family `trace`)* — per-node CPU time, poll counts and stall
  detection by consuming embassy-executor's trace hooks, with node *names* attached.

The supervisor deliberately does **not** allocate, own a HAL, manage power states, or know what your
tasks do — it orchestrates their *lifecycle* and leaves the rest to you. It also does **not**
catch panics: a panicking task is not captured or restarted (panic capture is off the table in a
`forbid(unsafe_code)` no_std library — it would need unwinding or the app-global panic handler).
Pair the supervisor with a hardware watchdog for crashes, and the `liveness` heartbeat for
alive-but-wedged tasks.

## The grammar, at a glance

The whole DSL is three item kinds — `node`, `pool`, `executor` — each one line, comma-
separated clauses, `;`-terminated. A useful graph is often just nodes, deps and task:

```text
supervisor_graph! {
    node NET  = Terminate, deps: [], task: net_task;
    node HTTP = Terminate, deps: [NET], task: http_worker;
}
```

Everything else is an optional clause on those same lines:

```text
supervisor_graph! {
    name: IDENT;                        // optional first item: rename the GRAPH static
    executor NAME;                      // a runtime-filled spawner slot (other core / IRQ tier)

    node NAME = Mode,                   // Mode: Terminate | Pause | OnDemand
        deps: [A, POOL, NET ready]      // bring-up order; `ready` also waits for set_ready()
        , task: worker                  //   or `spawn: task_fn`; omit both = app-spawned
        , resources: [R: Type, ..]      //   owned values threaded from main (kinds: local/shared/consume)
        , exit: Type                    //   capture the worker's return value
        , state: Type = expr            //   per-activation boxed state, freed on exit
        , cancel                        //   shell owns the shutdown race; worker takes no node
        , pool_size: N , executor: NAME , slot_timeout: MS , disabled;

    pool NAME = [Mode, ..],             // one Mode per member, floor first
        deps: [..], task: worker,
        resources: [..],                //   take kinds become per-member slot arrays
        policy: DeferredShrink::new(..),// scaling policy (required)
        min: EXPR, max: EXPR            //   trailing, in order: [, slot_timeout: MS][, cancel]
        , cancel;                       //   same flag, applied to every member
}
```

Reading rules, all regular: node clauses after `deps:` may appear **in any order**
(only `pool` fields are positional); the mode sits **after `=`**; every clause is **inline on
its item** (there are no block forms and no top-level `resources { }` section); a
pool always names its `policy:`; take-kind resource names are **globally unique**
(each is one static — only `shared` names repeat, by design); and `detached` is not a
mode but a runtime call (`TaskNode::set_detached`). Anything structurally wrong — a
dependency cycle, an unknown name, a duplicate — is a compile error with a message,
never a runtime surprise.

## Quickstart

```rust,ignore
use embassy_executor::Spawner;
use embassy_supervisor::{RunError, Supervisor, supervisor_graph};

// Declare the graph once: `supervisor_graph!` generates the node `static`s and a
// single `GRAPH` bundling the node slots, dep table, compile-time order, and pools.
// Each `task:` names a plain async worker fn (the macro stamps its
// `#[embassy_executor::task]` shell); `app` depends on `net`.
supervisor_graph! {
    node NET = Terminate, deps: [], task: net_task;
    node APP = Terminate, deps: [NET], task: app_task;
}

// Plain async fns taking the node first — no embassy attribute needed.
async fn net_task(node: &'static embassy_supervisor::TaskNode) { /* ... */ }
async fn app_task(node: &'static embassy_supervisor::TaskNode) { /* ... */ }

#[embassy_executor::task]
async fn supervisor_task(spawner: Spawner) {
    // Infallible: the order is precomputed, so a dependency cycle is a compile error.
    let sup = Supervisor::new(&GRAPH);
    // Bring-up in dependency order, then drive pools + runtime control forever;
    // returns only on error, which the app escalates (typically a panic into a
    // hardware-watchdog reset).
    match sup.run(spawner).await {
        RunError::Spawn(_) => panic!("bring-up failed"),
        RunError::Shutdown(e) => panic!("{} missed its shutdown ack", e.node.name),
    }
}
```

`run()` = `start()` + the driver loop; call the pieces yourself (`start`, then a
`select(run_pools, wait_control)` loop) when the driver must watch extra wake sources.
Bring-up is `async` because an `executor:` node first awaits its slot; a plain
single-executor graph resolves immediately — the `.await` costs nothing.

## The model

Three pieces, all `static`:

- **`TaskNode`** — one per managed task: a name, a `Mode`, an optional spawn fn, and a
  private handle of atomic flags + signals. The *task side* of the protocol is a handful of
  node methods — see [Writing supervised tasks](#writing-supervised-tasks-the-tasknode-api).
- **`Graph<N>`** — the macro-emitted `GRAPH`: `nodes` (fixed `[Option<&TaskNode>; N]` — a
  `#[cfg]`-ed-out node keeps its slot as `None`), `deps` (per-node dependency indices),
  `order` (the compile-time topological order), and `pools` (with the `pool` feature). The
  fields are public: a status endpoint can iterate them directly.
- **`Supervisor<N>`** — construction-free orchestration over `&GRAPH` (`new` is
  `const`, so `static SUP: Supervisor<5> = Supervisor::new(&GRAPH);` works; `N` =
  total graph slots, pool members included — the same `N` as `Graph<N>`), in three
  tiers: whole-graph, single-node, and cascading subsystem verbs.

The full verb surface, with signatures (every error type is `Debug` — `.unwrap()` /
`.expect()` work — and `defmt::Format` under the `defmt` feature):

| verb | signature |
|---|---|
| `start` | `async fn(&self, Spawner) -> Result<(), SpawnError>` — quiescent → running, any state |
| `run` | `async fn(&self, Spawner) -> RunError` — `start` + drive pools/control; returns only on error |
| `teardown` / `teardown_continue` | `async fn(&self) -> Result<(), ShutdownTimeout>` |
| `respawn_terminate` | `async fn(&self, Spawner) -> Result<(), SpawnError>` — wake pair, with... |
| `resume_pausable` | `fn(&self)` — ...this (sync: parked tasks pick up immediately) |
| `start_node` | `async fn(&self, &'static TaskNode, Spawner) -> Result<(), SpawnError>` |
| `stop_node` | `async fn(&self, &'static TaskNode) -> Result<(), ShutdownTimeout>` — awaits the ack |
| `resume_node` | `fn(&self, &'static TaskNode)` — sync, `Pause` nodes only |
| `activate` | `async fn(&self, &'static TaskNode, Spawner)` — cascade; spawn errors deliberately swallowed |
| `deactivate` | `async fn(&self, &'static TaskNode) -> Result<(), ShutdownTimeout>` — cascade |
| `apply_control` | `async fn(&self, ControlCommand, Spawner) -> Result<(), ShutdownTimeout>` |
| `run_pools` | `async fn(&self, Spawner) -> ShutdownTimeout` — completes only on error |

Error provenance: `RunError`, `ShutdownTimeout`, `Aborted`, `ControlQueueFull` are
crate types; `SpawnError` is re-used from `embassy_executor`. All the guarantees here
are cross-thread (release/acquire atomics) — a host test's main thread reads them as
safely as another task.

The control mailbox (feature `control`) is two free functions and two small types:
`async fn request_control(&'static TaskNode, ControlOp)` (lossless — awaits mailbox
capacity), `fn try_request_control(..) -> Result<(), ControlQueueFull>` (sync
contexts), and `enum ControlOp { Activate, Deactivate }` — just those two variants;
higher-level verbs (start/stop/pause/resume) fold onto them per the node's `Mode`.
All of these are importable from the crate root (`embassy_supervisor::try_request_control`,
`embassy_supervisor::ControlOp`, …); `ShutdownTimeout`'s one field is
`pub node: &'static TaskNode` (hence `e.node.name` in escalation messages).

One cascade asymmetry worth knowing: `activate` expands *dependencies* (up) and
`deactivate` expands *dependents* (down) — so a pool taken down as a **dependent** of
a deactivated node is not re-enabled by re-activating that node. Re-enable it by
targeting the pool itself: `Activate` on **any member** expands to the whole pool
(membership is part of the seed), respawning the floor and re-enabling the `OnDemand`
members for policy-driven growth.

`Mode` decides what each transition does to a node:

| mode | at boot | on teardown | on bring-up |
|---|---|---|---|
| `Terminate` | spawned | exits its loop (acks) | **respawned** (`respawn_terminate`) |
| `Pause` | spawned (or app-spawned if parked) | acks, then parks on `wait_resume()` | **resumed in place** (`resume_pausable`) — keeps held resources |
| `OnDemand` | not started | stopped like `Terminate` | not auto-started — pools/control start it |

How a task implements its half of these transitions is the
[TaskNode API](#writing-supervised-tasks-the-tasknode-api).

## Lifecycle reference

The canonical per-operation matrix — what each supervisor operation does to a node, by mode
and by the two lifecycle-spanning flags (`disabled`, `detached`). Other docs link here.

**Missed acks are errors, not panics.** Every stop path awaits the target's ack with a
2 s timeout; a node that misses it is returned as `ShutdownTimeout` naming the node —
from `stop_node`, `teardown`, and (feature `control`) `apply_control`; `run_pools`
completes (only) with it when a shrink hits a wedged member. `teardown` **aborts at the
first timeout** so a still-live dependent never has its dependencies stopped under it;
after `Err` the graph is partially down and the application escalates (hardware watchdog
reset, `panic!`, or retry). `teardown_continue` is the best-effort variant for the
"hardware reset next" path: it visits every remaining node past the wedge and reports
the first timeout at the end.

| operation | `Terminate` | `Pause` | `OnDemand` | disabled | detached |
|---|---|---|---|---|---|
| `start` *(boot + re-entry, async)* | spawned in dep order; already-running skipped (idempotent) | spawned (cold); an instance parked by an earlier `teardown` is **resumed in place**; a parked (no-`spawn:`) node is only marked running | skipped | skipped | first start spawns it (tasks detach *themselves* after that); re-entry skips it — its instance survived the teardown |
| `teardown` | shutdown + ack, exits | shutdown + ack, parks on `wait_resume()` | stopped if running, else skipped | already down — nothing to do | **skipped** (self-managed) |
| `deactivate` *(control)* | disabled + stopped; cascades to transitive dependents, dependents first | disabled + stopped, parks; stays parked | disabled + stopped — the whole pool, atomically | re-disabled (idempotent) | **skipped** — never pulled into the cascade, even when targeted directly |
| `activate` *(control)* | enabled + started, after its transitive deps | enabled + resumed in place | enabled only — the pool policy regrows it under load | this is the flag it clears | **skipped** — not re-enabled, not restarted; its `deps:` are start-ordering only and are not expanded |
| `stop_node` | shutdown + ack | shutdown + ack, parks (**this is the single-node pause**) | shutdown + ack (the pool-shrink path) | not running → no-op | **no-op** |
| `resume_node` | no-op (wrong mode) | reset + resumed in place, keeps held resources | no-op (wrong mode) | skipped — a manual pause sticks | **no-op** |
| `respawn_terminate` *(async)* | reset + respawned in dep order | untouched (use `resume_pausable`) | left down — the policy regrows it | skipped — a manual stop sticks | **skipped** — it never went down, respawning would double-spawn |
| `resume_pausable` | untouched | reset + resumed in place, keeps held resources | untouched | skipped — a manual pause sticks | **left parked** |

Two flags cut across the modes:

- **`disabled`** is the "a human said stop" latch: `deactivate` sets it, `activate` clears it,
  and every bring-up path honors it so a manual stop/pause survives a wake respawn or an
  elastic regrow.
- **`detached`** (`TaskNode::set_detached(true)`) is full hands-off: the node manages its own
  lifecycle and the supervisor never drives it again. Its `deps:` still order its *first*
  spawn — after that, the graph only remembers where it was declared.

**Defaults in one place:** shutdown-ack timeout **2 s** (a missed ack returns
`ShutdownTimeout`); pre-spawn slot/gate/ready-dep wait **100 ms** per gate
(override per node with `slot_timeout:`; timeout = `SpawnError::Busy`); control
mailbox depth **4** (`request_control` awaits capacity, `try_request_control`
reports `ControlQueueFull`); trace registries track up to **4** executors and **4**
graphs.

## Writing supervised tasks (the TaskNode API)

> **A worker typed `-> !`** (or a service contract returning a `Never` type) opts out
> of `Terminate`/restart *by type* — the body can never return, so stop and respawn
> semantics are inert on it as-is. Two ways in: add
> [`cancel`](#cancel--supervisor-unaware-workers) to the node and the generated shell
> races the body against shutdown for you (no signature change, the future is dropped
> in place), or keep the node argument and race the work yourself (one
> `run_cancellable` call) when teardown needs ordered post-cancel work.
> `Pause`-parked and detached daemons are the forms that legitimately never return.

A supervised task is an async fn whose first parameter is its node — the macro's glue
passes it automatically; extra arguments come from the partial-call form
(`task: my_task(EXTRA)`). (A [`cancel`](#cancel--supervisor-unaware-workers) node is
the exception: its worker never sees the node — the shell holds it and uses this same
API on the worker's behalf.) The preferred style is a **plain worker fn** declared with
[`task:`](#task--generated-shells-for-plain-or-generic-workers) — the graph stamps the
`#[embassy_executor::task]` shell for you:

```rust,ignore
async fn my_task(node: &'static TaskNode) { /* ... */ }
```

Alternatively, write the attribute yourself and declare the fn with `spawn:` — needed in a
few situations ([which to use](#spawn-vs-task--which-to-use)). Everything below (the four
rules, the method table) applies identically to both styles; only who writes the
`#[embassy_executor::task]` differs.

The node is the task's half of the lifecycle protocol. Four rules cover all of it:

1. **Select your work against `wait_shutdown()`** at every await point that can block
   indefinitely — that's how a teardown/stop reaches you.
2. **Ack exactly once per stop** with `ack_dropped()`: on exit (`Terminate`/`OnDemand`),
   or on each pause (`Pause`) *before* parking. A task that never acks surfaces as a
   `ShutdownTimeout` error naming the node — a loud bug report, not a hang, and the
   application chooses the escalation.
3. **An autonomous exit calls `mark_exited()`** — it acks like `ack_dropped()` *and*
   records the completion, so a worker that returns on its own reads as down
   (`has_exited()`, not running-forever) and a control `Activate` can respawn it.
   `task:` shells do this automatically after the worker returns; only hand-written
   `spawn:` tasks call it themselves.
4. **Resources follow the mode**: a `Terminate` task re-acquires everything on respawn
   (drop-on-exit is the cleanup); a `Pause` task keeps what it holds across
   pause→resume and never re-acquires.

Task-side methods:

| method | role |
|---|---|
| `run_cancellable_acked(fut).await` | the everyday body: race `fut` against shutdown AND complete the handshake on `Err(Aborted)` — discarding the result (`let _ =`) is fine, the ack already happened |
| `run_cancellable(fut).await` | same race, no ack — run cleanup between the cancellation and your own `ack_dropped()` |
| `wait_shutdown().await` | the underlying primitive: park until a stop/pause is requested (immediate if already requested) |
| `ack_dropped()` | complete the handshake: clears `running`, wakes the supervisor's ack wait |
| `mark_exited()` | `ack_dropped()` + record the completion (`has_exited()`) — call on an autonomous exit; `task:` shells emit it automatically |
| `wait_resume().await` | `Pause` only: park (after acking) until resumed |
| `mark_busy()` / `mark_idle()` | pool workers: report load; a *real* transition fires the scale signal itself — no manual `request_scale()` needed |
| `shutdown_requested()` | synchronous check, e.g. at the loop top before starting new work |
| `has_exited()` | true once the last instance's body returned; cleared by the pre-spawn reset |
| `set_detached(true)` | opt out of supervision from now on (self-managed daemon or run-once — see the [lifecycle reference](#lifecycle-reference)) |
| `adopt(&token)` | parked nodes: register a hand-spawned task's id so trace accounting sees it |

The combinators return `Result<F::Output, Aborted>`; `Aborted` is a crate type —
`use embassy_supervisor::{Aborted, TaskNode};` covers the canonical loops.

**Status methods** — readable from anywhere (a status endpoint iterates `GRAPH.nodes`
and reads these; all are cheap atomic loads):

| method | true when |
|---|---|
| `is_running()` | the supervisor has an instance up (spawned, not acked, not exited) — "did it come up / go down" checks read this |
| `is_busy()` | the instance reported `mark_busy()` (pool load) |
| `is_disabled()` | stopped-at-boot or control-`Deactivate`d, and not yet re-activated |
| `is_detached()` | self-managed; every lifecycle op skips it |
| `has_exited()` | the last instance's body returned (recorded by the `task:` shell / `mark_exited`); cleared by the pre-spawn reset |
| `shutdown_requested()` | a stop/pause was requested — set at the request, readable until the next pre-spawn reset (so a parked `Pause` node still reads `true`) |
| `is_ready()` *(feature `readiness`)* | the task asserted `set_ready()` (cleared by `clear_ready()` and the pre-spawn reset) |
| `is_stale(max_age)` *(feature `liveness`)* | running but no `beat()` within `max_age` |

Useful compositions: **down** = `!is_running()`; **parked `Pause`** = mode `Pause` +
`!is_running()` + `shutdown_requested()`; **autonomous completion** = `has_exited() &&
!shutdown_requested()`. A spawn that fail-closed (`SpawnError::Busy` from a gate)
leaves the node `!is_running()` — nothing was taken or spawned. Ordering guarantees: when `stop_node`/`teardown`/`deactivate`
return `Ok`, the ack has happened and `is_running()` is already `false`; for bodies
that ack by returning (the `run_cancellable_acked` idiom), `has_exited()` is also
already `true` — the ack and the return land in the same poll.

**`Terminate` / `OnDemand` worker** — the canonical cancellable loop:

```rust,ignore
#[embassy_executor::task]
async fn worker_task(node: &'static TaskNode) {
    let mut conn = acquire();                    // re-acquired on every respawn
    loop {
        match node.run_cancellable_acked(conn.serve()).await {
            Ok(res) => handle(res),
            Err(Aborted) => return,              // acked; drop(conn) is the cleanup
        }
    }
}
```

The combinators own the select rule 1 asks for. Use bare `run_cancellable` when
cleanup must run between the cancellation and the ack (flush, unpublish, busy/idle
bracketing); keep a hand-written `select3` when the loop races more than work vs
shutdown — nesting combinators there buys nothing.

**`Pause` node** — ack, then park; held resources survive:

```rust,ignore
#[embassy_executor::task]
async fn sensor_task(node: &'static TaskNode) {
    let mut bus = acquire_once();                // kept across pause/resume
    loop {
        while let Ok(v) = node.run_cancellable(sample(&mut bus)).await {
            publish(v);
        }
        node.ack_dropped();                      // ack the pause...
        node.wait_resume().await;                // ...then park, still owning `bus`
    }
}
```

**Pool worker** — same as `Terminate`, plus load reporting around the busy section:

```rust,ignore
node.mark_busy();                                // idle→busy fires the scale signal
serve_connection(&mut socket).await;
node.mark_idle();                                // busy→idle fires it again
```

Keep `mark_busy()` held for the whole session the worker's resource is tied up (e.g. a
keep-alive connection): the policy only shrinks non-busy workers. The connection-bound
worker that must stay busy *across* a possible cancellation composes the pieces like
this — busy for the whole serve, ack only after the bracketing:

```rust,ignore
loop {
    match node.run_cancellable(socket.accept(PORT)).await {
        Err(Aborted) => return node.ack_dropped(), // idle here: nothing to bracket
        Ok(conn) => {
            node.mark_busy();
            let served = node.run_cancellable(serve(conn)).await; // busy across the race
            node.mark_idle();
            if served.is_err() {
                return node.ack_dropped(); // cancelled mid-serve: bracket, THEN ack
            }
        }
    }
}
```

**Detached daemon / run-once** — detach as the first act, then own your lifecycle:

```rust,ignore
#[embassy_executor::task]
async fn confirm_task(node: &'static TaskNode) {
    node.set_detached(true);                     // supervisor is hands-off from here
    wait_until_ready().await;
    confirm();                                   // runs once and simply returns
}
```

**Parked node** (declared with no `spawn:`) — the app spawns it by hand, typically because
it needs values only `main` owns; `adopt` keeps trace attribution working:

```rust,ignore
let token = pump_task(&PUMP, hw_handle).unwrap(); // task fns return Result<SpawnToken, _>
PUMP.adopt(&token);                               // register its task id for trace
spawner.spawn(token);                             // Spawner::spawn takes the token
```

## The `supervisor_graph!` DSL

```text
executor NAME;                        // runtime-filled SendSpawner slot (tier / second core)
node NAME = Mode, deps: [A, B][, executor: EXEC], spawn: <spawn>[, disabled];
node NAME = Mode, deps: [A, B][, executor: EXEC], task: <worker>[, pool_size: N]
    [, resources: [[#[cfg(..)]] RES: [local] [shared|consume] Type, ..]]
    [, slot_timeout: MS][, cancel][, disabled];
node NAME = Mode, deps: [A];          // neither => parked node the app spawns itself
pool NAME = [Mode, ..], deps: [A][, executor: EXEC],
    spawn: <fn> | task: <worker>,
    [resources: [RES: [local] [shared|consume] Type, ..],]
                                        // take kinds → per-member slot arrays;
                                        // shared (incl. shared local) one pool-wide slot
    policy: [<Type> =] <expr>,
    min: N, max: M[, slot_timeout: MS][, cancel];
```

### Spawn forms

A bare path `f` spawns `f(&NAME)`; a partial call `f(a, b)` spawns `f(&NAME, a, b)` (the node
is always injected first — except under [`cancel`](#cancel--supervisor-unaware-workers),
which suppresses it); a closure is emitted verbatim (nodes only). These forms apply to
both `spawn:` (a hand-written `#[embassy_executor::task]` fn) and `task:` (a plain worker fn
the macro wraps) — **prefer `task:`**; see
[`spawn:` vs `task:`](#spawn-vs-task--which-to-use) for the cases where `spawn:` is the
right tool. Omit both for a **parked** node whose task the application spawns itself (e.g. a
`Pause` sensor holding a peripheral handle) — the supervisor tracks it but never spawns it.

### `task:` — generated shells for plain (or generic) workers

`spawn:` names a hand-written `#[embassy_executor::task]` fn. `task:` instead names a **plain
async fn** — possibly generic — and the macro stamps the concrete
`#[embassy_executor::task]` shell for you. This is the escape hatch for embassy's
"task functions must not be generic" rule (one static `TaskPool` per concrete future type):
write the worker once, declare one node per concrete instantiation, and each declaration gets
its own monomorphized shell.

```rust,ignore
async fn sensor<D: Sensor>(node: &'static TaskNode, dev: D) { /* ... */ }

supervisor_graph! {
    node BME = Terminate, deps: [BUS], task: sensor::<Bme280>(bme_dev());
    node SHT = Terminate, deps: [BUS], task: sensor(sht_dev());   // turbofish optional
}
```

Semantics:

- Same path / partial-call forms as `spawn:` (no closures — the shell needs a name to call).
- **Worker args are evaluated inside the shell**, at the task's first poll, on the node's own
  executor — so the DSL never needs the arg types, an `executor:`/second-core node builds its
  resources on the core that runs them, and cross-node data should go through awaited
  accessors (a spawn batch polls last-first). Corollary: an extra that can be **missing** at
  first poll is a task-side panic, not a failed spawn — extras are for infallible accessors.
  A value that might not exist yet belongs in `resources:` (a `shared` entry for a fan-out
  handle): the pre-spawn gate turns "missing" into a clean `SpawnError::Busy`.
- `pool_size: N` (default 1) sizes the shell's `TaskPool` — headroom for a respawn issued
  while the previous instance is still draining.
- On a `pool`, `task:` emits ONE shell sized to the member count.
- Trace adoption and `executor:` routing compose exactly as with `spawn:`.
- The ceiling embassy imposes still stands: concrete types are fixed per binary — `task:`
  removes the boilerplate, not the monomorphization.

### `spawn:` vs `task:` — which to use

**Prefer `task:`.** It drops the `#[embassy_executor::task]` boilerplate, admits generic
workers, sizes a pool's `TaskPool` from the member count automatically (no
`pool_size = MAX` constant to keep in sync with the DSL's `max:`), and is the only form
that supports `resources:`. The generated shell is free at runtime: its wrapper inlines
into the same poll, and its `TaskPool` static simply replaces the one the attribute would
have emitted.

`spawn:` remains the right tool in four situations:

1. **The task fn already carries `#[embassy_executor::task]` and you can't (or shouldn't)
   strip it** — it lives in another crate, or other code depends on it staying a task fn.
   `task:` needs a *plain* async fn to wrap; a token-returning task fn can't be re-wrapped.

   ```rust,ignore
   // other_crate exports: #[embassy_executor::task] pub async fn modem_task(..) { .. }
   node MODEM = Terminate, deps: [], spawn: other_crate::modem_task(&NODES[0]);
   ```

2. **The same task is also spawned outside the graph.** `spawn:` reuses the one existing
   `TaskPool`; `task:` would stamp a second shell + pool — duplicate RAM for the same
   future type.

   ```rust,ignore
   #[embassy_executor::task(pool_size = 2)]
   async fn logger(node: &'static TaskNode, sink: Sink) { /* ... */ }

   // One instance supervised ...
   node LOG = Pause, deps: [], spawn: logger(uart_sink());
   // ... and one spawned by hand elsewhere, sharing logger's pool:
   spawner.spawn(logger(&NODES[log_idx], usb_sink()).unwrap());
   ```

3. **Custom spawn-time logic** — the verbatim closure form (nodes only). `task:` rejects
   closures (the shell needs a name to call).

   ```rust,ignore
   node SENSOR = Terminate, deps: [BUS],
       spawn: |s: Spawner| {
           let token = sensor_task(&SENSOR, if fast_variant() { Odr::Hz30 } else { Odr::Hz8 })?;
           SENSOR.adopt(&token);   // closures bypass the macro's trace glue — adopt by hand
           s.spawn(token)
       };
   ```

   ⚠️ The `adopt` line is **your job, and nothing will remind you**: the closure owns the
   `SpawnToken`, so the macro cannot capture the task id (`trace`) or stamp the node name
   (`metadata-names`) for you, and a stable proc-macro cannot emit a warning. Forgetting it
   is silent — the node simply never appears in the trace/name output.

4. **Arguments that must be evaluated at spawn time, on the supervisor's executor.**
   `spawn:` partial-call args run in the spawn glue, at the moment of the (re)spawn;
   `task:` extras run inside the shell at its *first poll, on the node's own executor*.
   The `task:` behavior is what you usually want (an `executor:`/second-core node builds
   its state on the core that runs it) — reach for `spawn:` when an argument snapshots
   something that must be read *now* or must not run on the target tier.

   ```rust,ignore
   // Snapshot the respawn count at the moment of this spawn, not at first poll
   // (an interrupt-tier node's first poll can preempt and land arbitrarily later):
   node REPORT = Terminate, deps: [], executor: HIGH, spawn: report_task(boot_epoch());
   ```

Omitting both keeps the node **parked** (see [Spawn forms](#spawn-forms)) — that's a third
option, not a tie-breaker between the two.

### `resources:` — safe resource threading

`ResourceSlot<T>`'s full hand-usable API, for reference (the macro's glue uses the
same calls): `provide(T)` / `restore(T)` fill the slot (restore is provide, named for
the give-it-back half), `take() -> Option<T>` empties it, `get() -> Option<T>`
(`T: Copy` only) copies without emptying, and `async wait_take() -> T` awaits a fill
then takes — how an `exit:` slot is read. `provide` on an already-filled slot
overwrites (the old value is dropped): every slot is a mailbox, not a log.

By default a supervised task that needs a peripheral re-acquires it inside its body
(`Peripherals::steal()`), giving up embassy's compile-time ownership guarantee.
`resources: [NAME: Type, ..]` (requires `task:`; node-only) restores it: each entry emits a
`pub static NAME: ResourceSlot<Type>` at the declaration site, and `main` **moves** the
resource in:

```rust,ignore
async fn blink(node: &'static TaskNode, led: &mut Output<'static>) { /* ... */ }

supervisor_graph! {
    node BLINK = Terminate, deps: [], task: blink,
        resources: [LED: Output<'static>];
}

// main, after the Peripherals split:
LED.provide(Output::new(p.PIN_25, Level::Low)); // consumes p.PIN_25 — no steal, no 2nd owner
sup.start(spawner).await?;
```

The protocol, per (re)spawn:

1. `main` `provide()`s the value once. Consuming the `Peripherals` field is the
   **compile-time exclusive-ownership guarantee** — a second owner cannot exist.
2. The generated glue `take()`s it just before the spawn. An unprovided slot fails
   `Supervisor::start` with `SpawnError::Busy` after a bounded wait (the supervisor logs the
   node name) — fail-closed at bring-up, not a panic inside a running task. Provisioning is
   the runtime-checked half of the contract.
3. The generated shell hands the worker `&mut Type` — after the node arg, in declared order,
   before any partial-call extras — and `restore()`s the value after the worker returns
   (i.e. after its shutdown ack). A Terminate respawn therefore re-takes the **same
   instance**; a Pause worker never returns, so it simply retains its resources.

The supervisor awaits a node's slots being filled before each (re)spawn (same bounded wait
as `executor` slots), so late provisioning and the respawn-vs-restore window on another core
are both covered. Caveats: a panic in the worker skips the restore (embedded panic = reboot);
`pool_size > 1` on a `resources:` node buys nothing (the slot holds ONE value — a second
concurrent spawn fails at `take()`); pools reject `resources:` (members would contend for a
single instance).

#### Resource kinds: `local`, `consume`, and `shared`

Per-entry markers (order-free; `local` composes with either of the mutually exclusive
`consume`/`shared`) refine the default lend-and-restore protocol for the resources it
cannot express:

| kind | worker receives | on worker exit | use for |
|---|---|---|---|
| *(default)* | `&mut Type` | `restore()`d — respawn re-takes the same instance | long-lived singletons (`Output`, a reborrowable `Peri`) |
| `consume` | `Type` **by value** (glue `take()`s) | nothing — the slot stays **empty** | resources the worker must *drop* at teardown (a driver whose `Drop` releases pins/DMA) or that go stale across a power cycle and are rebuilt each run |
| `shared` | `Type` **by value** (glue **copies** via `get()`, `T: Copy`) | nothing — the slot **stays filled** | one handle fanned out to many consumers (`embassy_net::Stack`, a `&'static` shared-bus ref); several nodes — and whole `task:` pools — declare the SAME slot name |
| `local` | as the kind it composes with | as the kind it composes with | `!Send` values (`RefCell`-/`NoopRawMutex`-based driver handles) on a **single core** |

`consume` makes teardown-drop explicit and turns the wake path into "build fresh, `provide()`,
respawn": until the application re-provides, a respawn fail-closes with `SpawnError::Busy`
instead of reusing a stale instance.

`shared` replaces the panicking-accessor pattern for fan-out handles: instead of a
`task:` extra like `stack()` that panics at first poll when the value is missing, a
`shared` resource is gate-awaited before the spawn and a missing value is a clean
`SpawnError::Busy`. The slot static is emitted once per unique name (with the union of
the declaring sites' `#[cfg]` predicates); every re-declaration must repeat the same
kind markers and type. Entries may also carry per-entry `#[cfg(...)]` — gate the worker
fn's matching parameter with the same attribute.

`local` **requires the non-default `local-resources` feature**: it swaps the emitted
`ResourceSlot` for a graph-site slot type without the `T: Send` bound, and that type
carries an `unsafe impl Sync` — the one graph form that injects unsafe
code, hence the explicit opt-in (same reason the `trace-hooks` symbols live at the graph
site). Its soundness contract is: all `provide`/`take`/`restore` of a given slot happen on
ONE core. Without the feature a `local` marker is a compile error naming it; the macro also
rejects `local` + `executor:` (a `SendSpawner`-routed node needs a `Send` future), and a
consumer crate that forbids `unsafe_code` cannot use `local`.

```rust,ignore
// The cyw43 pattern: a !Send radio runner, dropped at teardown to release its
// pins, rebuilt by the app before each wake respawn.
async fn radio(node: &'static TaskNode, runner: Cyw43Runner) {
    let _ = node.run_cancellable_acked(runner.run()).await; // drop releases PWR/PIO/DMA
}

supervisor_graph! {
    node RADIO = Terminate, deps: [], task: radio,
        resources: [RUNNER: local consume Cyw43Runner];
}

// bring-up (and again on every wake cycle, BEFORE the respawn):
RUNNER.provide(build_radio_runner().await);
```

### `exit:` — typed exit values

`exit: Type` on a `task:` node emits `pub static <NODE>_EXIT: ResourceSlot<Type>`; the
generated shell `provide()`s the worker's **return value** into it just before
recording the exit, so `has_exited()` implies the value is present. Read it with
`<NODE>_EXIT.wait_take().await` (or non-blocking `take()` after `has_exited()`). The
slot is a **mailbox, not a log**: the next completion overwrites an unread value.

The idiom for completed-vs-cancelled is a worker whose body *is* the combinator:

```rust,ignore
async fn serve_worker(node: &'static TaskNode) -> Result<Outcome, Aborted> {
    node.run_cancellable_acked(serve()).await   // exit: Result<Outcome, Aborted>
}
```

`task:`-only (the shell is what captures the return; a `spawn:` fn can `provide()`
into an app-declared slot itself) and not available on `pool` (K members share one
shell; per-member exit values would need per-member storage).

**A worker that can never return rejects `exit:` at compile time.** A `-> !` worker
has no output, so the slot could never be filled and a `wait_take()` on it would hang
forever; the shell's `provide()` denies `unreachable_code` on itself to catch that.
rustc reports it as `unreachable statement` (or `unreachable call` under
[`cancel`](#cancel--supervisor-unaware-workers)) pointing **at the `exit:` clause** —
drop the clause, or give the worker a return type it can actually reach. A diverging
worker *without* `exit:` stays perfectly legal: that is the shape `cancel`, `Pause`,
and detached daemons exist for.

### `cancel` — supervisor-unaware workers

Every worker so far takes the node as its first argument and answers the shutdown
handshake itself (`run_cancellable*`, or just returning). `cancel` moves that whole
job into the generated shell: the shell drives the worker under
[`run_cancellable`] and does **not** pass it the node, so the worker is a plain
`async fn` — the shape an existing firmware already has:

```rust,ignore
// No node, no handshake, no supervisor in sight. Loops forever.
async fn telemetry(uart: &mut Uart<'static, Async>) -> ! {
    loop { /* ... */ }
}

supervisor_graph! {
    node TELEM = Terminate, deps: [NET], task: telemetry, cancel,
        resources: [UART: Uart<'static, Async>];
}
```

On `stop_node`/teardown the shell drops the worker's future in place and runs its
usual tail: state freed, resources `restore()`d, exit recorded — so a `Terminate`
respawn re-takes the same instances, exactly as if the worker had been node-aware.
Resources still arrive, now as the *first* arguments (nothing leads them).

With `exit:`, the value is provided only on a **real completion**; an aborted
worker leaves `<NODE>_EXIT` empty (with `shutdown_requested()` set), which is how
a waiter tells "finished" from "stopped". Combining `exit:` with a worker that can
*never* complete is [a compile error](#exit--typed-exit-values). Drop-in-place is
also the trade: the
worker gets no post-cancel code, so a task that must flush or release something
*ordered* at teardown should keep the node argument and race
`run_cancellable_acked` itself.

A `pool` takes the same flag, as its **last** clause (the pool grammar is
positional: after `max:`, and after `slot_timeout:` if present). It applies to the
one shell all members share, so it applies to every member:

```rust,ignore
async fn handler(conn: &mut Conn) -> ! { loop { /* serve */ } }

supervisor_graph! {
    pool WORKERS = [Terminate, OnDemand, OnDemand], deps: [NET], task: handler,
        resources: [CONN: Conn],
        policy: DeferredShrink::new(Duration::from_secs(5)),
        min: 1, max: 3, cancel;
}
```

An elastic **shrink** is a stop like any other, so this is what lets the policy
retire a worker that would never have acked one: the member's future is dropped in
place and the shell restores its per-member resource to *its own* slot index, ready
for the regrow. The load signal is the one thing that has to move outside the
worker — a `cancel` member holds no node, so `mark_busy()`/`mark_idle()` are called
on `WORKERS[i]` by whatever hands it work (the member statics are app-visible), not
by the worker itself.

`task:`-only (a `spawn:` fn owns its body and can call `run_cancellable` itself),
and rejected on `Mode::Pause` because a Pause worker must survive the stop and park on
`wait_resume()`, which an exit record nothing ever resumes contradicts.

### `disabled`

Declared but not started at boot; a control `Activate` starts it later (e.g. an OTA task).

### `executor NAME;` and `executor: NAME`

`executor NAME;` emits a `SpawnerSlot` static; the app fills it with a `SendSpawner`
(`InterruptExecutor::start()`, `Spawner::make_send()`), and annotated nodes spawn through it.
`start()` awaits the slot (bounded) as part of bring-up; a slot still empty at the deadline
fails the spawn with `SpawnError::Busy` — loud, not silent. Constraints: `executor:` requires
a `spawn:` fn (it cannot combine with a verbatim closure), and the routed task's future must
be `Send`.

### Dependencies

`deps:` names declared nodes *or pools*. A pool name resolves to the pool's **floor member**
(member 0, the `min`-kept one), so `deps: [POOL]` means "start after the pool is up".

**A plain dep orders *spawns*, not *readiness*.** `start()` spawns a node and immediately
marks it running, so a dependent with no gates can race its provider's body. Bring-up
walks the topological order sequentially: a node's gate wait (slot, resource, ready
dep) blocks every node after it in the order until it resolves or times out. Two
rendezvous exist, both opt-in:

- a `resources:` slot wait — the provider-node pattern (`provide()` after DHCP etc.);
- a **`ready` dep marker** *(feature `readiness`)*: `deps: [NET ready]` additionally
  awaits the dep's task-asserted `set_ready()` before spawning this node, bounded by
  this node's `slot_timeout` (then `SpawnError::Busy`, with a log line naming the
  not-ready dep). Elastic-pool growth also defers while a ready-marked dep is
  un-ready (a sync check per evaluation — no wait). `ready` on a pool name means the
  floor member's readiness; markers on a `pool`'s own `deps:` apply to every member.

The provider side is three calls: `set_ready()` once serving, `clear_ready()` on a
lost link (**status, not control** — dependents are not stopped; pair with a control
`Deactivate` for a cascade), and the pre-spawn reset clears it so a respawned provider
re-asserts. `wait_ready()` exists for app code too, with the same
single-pre-fill-waiter caveat as the other latching gates — fan N waiters out through
an app-owned `embassy_sync::watch::Watch` instead.

### `#[cfg(...)]`

Allowed on any `node`/`pool` *and on individual deps*. Absent nodes keep their slot as `None`
and are skipped everywhere at runtime.

### `pool`

The mode list declares the members (floor first: typically `[Terminate, OnDemand, ...]`). The
macro generates the member array `NAME: [TaskNode; K]`, per-member spawn glue, a
`NAME_POOL: ElasticPool<P>`, and the structural constants `NAME_MIN` / `NAME_MAX` /
`NAME_MEMBERS` (`usize`).

**Per-member resources.** Take-kind `resources:` entries (the default lend, and
`consume`) become **per-member slot arrays** — `pub static RES: [ResourceSlot<T>; K]`,
member `I` takes and restores element `I` exclusively, so members never contend, the
floor comes up with only floor-many elements provided, and a lend value survives a
shrink/regrow **on the same index** (the per-connection-worker shape). `shared`
entries — `shared local` included — stay one fan-out slot for the whole pool; only
take-kind `local` is rejected on pools (the single-core slot contract + per-member
restore is deferred).
A worker derives its own index from its node via
`NAME_POOL.member_index(node) -> Option<usize>` (`None` = not a member of this pool)
to reach per-member app state without per-member spawn arguments.

**`min:`/`max:` accept const expressions** (`min: FLOOR`, `max: FLOOR + 1`): integer
literals are validated at expansion time with exact spans; anything else makes the
emitted `NAME_MIN`/`NAME_MAX` consts the source of truth, guarded by
`const _: () = assert!(..)` (min ≤ max ≤ members ≤ 255). **The member count `K` (the
mode list) stays a literal by necessity**: it determines how many nodes, shells, name
strings, and graph slots are *emitted*, and a proc macro cannot evaluate a downstream
`const` — deriving the member count from a const is structurally out of reach. Pool fields are positional and fixed:
`deps → executor? → spawn → policy → min → max`. `policy:` takes the scaling policy; annotate
the type explicitly (`policy: DeferredShrink = make_policy()`) when the value isn't a
`Type::new(..)` constructor.

The constants exist for downstream **const-context sizing** — deriving a related capacity
from the DSL instead of duplicating the number by hand (a `const` cannot read the member
`static` array, so `NAME.len()` doesn't work there):

```rust,ignore
// One TCP socket per concurrently-running worker, plus one for DNS:
pub const SOCKET_BUDGET: usize = HTTP_MAX + 1;
let resources = StackResources::<SOCKET_BUDGET>::new();
```

### Limits and compile-time validation

At most **256 slots** per graph — all graph indices are `u8`, which keeps the dep table and
order arrays byte-sized on flash-constrained targets.

The macro rejects an invalid graph at compile time, each with a spanned error at the
offending token:

- **unknown dependency** — a `deps:` name that is not a declared node or pool
- **duplicate dependency** — `deps: [A, A]` (compared by resolved slot, so a repeated
  pool name counts too)
- **duplicate node/pool name** — a redeclared name would silently rewire earlier deps
- **unknown `executor:` name** — on a node or pool, checked against declared
  `executor NAME;` slots
- **`executor:` with a closure spawn** — the closure owns the spawn, so routing through a
  slot must happen inside it; only the task-fn-path forms combine with `executor:`
- **malformed spawn form** — anything other than a task-fn path, a partial call, or a
  closure
- **`task:` and `spawn:` together** — mutually exclusive per node/pool
- **a closure in `task:`** — the generated shell needs a worker fn it can name
- **`pool_size:` without `task:`** (or `pool_size: 0`) — it sizes the generated shell's
  `TaskPool`; a hand-written task fn declares its own
- **`resources:` without `task:`** — resources are taken/restored by the generated shell; a
  hand-written `spawn:` fn manages its own arguments
- **empty `resources:` list / duplicate resource name** — slot names are statics, unique
  across the whole graph (only `shared` entries may repeat a name, verbatim)
- **pool `resources:` without `task:`** — the generated shell receives the values (and
  restores lend entries); a `spawn:` task fn manages its own arguments
- **take-kind `local` on a `pool`** — the single-core slot contract + per-member restore
  is deferred; `shared local` is fine (one pool-wide fan-out slot)
- **a repeated kind marker on a `resources:` entry** (`consume consume T`) — declaration bug
- **`local` without the `local-resources` feature** — the kind emits an `unsafe impl Sync`,
  so it is strictly opt-in
- **`shared` with `consume`** — contradictory: one exclusive owner vs any number of copies
- **a `shared` slot re-declared with different kinds/type** — every declaration of the
  same name is ONE static and must repeat its shape verbatim
- **`local` resources with `executor:`** — on a node or a pool: a local slot carries
  `!Send` values; a `SpawnerSlot`-routed spawn needs a `Send` future
- **`slot_timeout: 0`** — would fail every gated spawn instantly
- **[`cancel`](#cancel--supervisor-unaware-workers) without `task:`** — on a node or a
  pool: the flag rewrites how the *generated* shell calls the worker; a hand-written
  `spawn:` fn can call `node.run_cancellable(..)` itself
- **`cancel` with `Pause`** — the node mode or any pool member: a Pause worker must
  survive the stop and park on `wait_resume()`, but `cancel` drops its future and
  records an exit; use `Terminate`/`OnDemand`, or drive the pause by hand
- **[`exit:`](#exit--typed-exit-values) on a worker that can never return** — the shell's
  `provide` would be dead code, so nothing could ever fill the slot and every waiter
  would hang; rustc reports it as `unreachable statement` (or `unreachable call` under
  `cancel`) spanned on the `exit:` clause rather than as a macro error. A diverging
  worker *without* `exit:` stays legal
- **pool bounds** — `min <= max <= K` (member count), values must fit `u8`
- **pool without the `pool` feature** — a `pool` item requires enabling it
- **more than 256 slots** — the `u8` index cap above
- **dependency cycle** — caught by the `const` topological sort, so it surfaces at
  const-eval of `GRAPH` rather than at macro expansion; still a compile error

Generated surface at the call site: one `pub static` per node, the pool array + `NAME_POOL`
\+ the `NAME_MIN`/`NAME_MAX`/`NAME_MEMBERS` consts,
one `SpawnerSlot` static per `executor NAME;`, one slot static per `resources:` entry (plus,
iff any entry is `local`, the local slot type), and `pub static GRAPH` — nothing else.

## Recipes by use case

### Heap and the graph

The reclaimability boundary, stated plainly: **task storage stays static — by
soundness, not preference.** Every `Waker` embassy hands out is an unrefcounted raw
pointer into the task's storage; stale wakes against *reused* storage are safe no-ops,
against *freed* storage they are use-after-free, and nothing counts outstanding waker
clones — so no safe free point exists. What IS reclaimable is
**future-owned state**: everything the body owns drops when it returns (embassy never
force-cancels — after the shutdown select, the future runs to completion). The
supervisor therefore uses heap only where it comes back:

- **`state: Type = init_expr`** *(feature `heap-state`)* — per-activation boxed state.
  The spawn glue fallibly boxes the init value (**alloc failure = `SpawnError::Busy`**,
  nothing spawned or stranded, retry when heap frees up), the shell lends the worker
  `&mut Type` (after resources, before extras — e.g. a node with
  `resources: [STACK: shared u32], state: Buf = Buf::new()` has a worker
  `async fn w(node: &'static TaskNode, stack: u32, buf: &mut Buf)`), and the Box
  **drops on task exit** —
  before restores and the completion record, so `has_exited()` implies the heap is
  back. Every activation allocates fresh; N respawns = net zero. On a pool, each
  member boxes its own. The container (the future's static storage) is now a thin
  shell; the bulk is paid only while the phase runs. The ~6-line fallible-boxing
  helper is the feature's entire `unsafe` surface and is emitted into YOUR crate
  (the `local-resources` precedent); you need a `#[global_allocator]`.
- **`consume Box<T>` slots** — the app-provided variant, zero crate support needed:
  provide a fresh `Box` before each activation, the worker owns it, drop-on-exit
  frees it, the slot stays empty until re-provided (fail-closed respawn). Use it when
  the *app* decides the allocation (budget checks before an `Activate` — pair with a
  free-bytes gate); use `state:` when the graph should just do it.
- **Lend a `Box`** (`RES: Box<Big>` with the default kind) to keep ONE allocation
  alive across respawns instead — pay-once, reclaim-never, but no per-cycle churn.

`Box<T>` in any of these still requires `T: 'static` — placement recipes, not
lifetime escape hatches.

### Subordinate sub-graph under an app state machine

The graph does not have to own your `main`. A state-machine (or super-loop) firmware
keeps owning sequencing — and carries data between states, which a declarative graph
cannot — while a **dedicated named sub-graph** is cycled with whole-graph ops per
state entry/exit, dependency-ordered both ways automatically:

```rust,ignore
supervisor_graph! {
    name: UPLOAD_GRAPH;
    node WIFI   = Terminate, deps: [],     task: wifi_ctrl,
        resources: [WIFI_HW: consume WifiController<'static>];
    node NET    = Terminate, deps: [WIFI], task: net_runner;
    node UPLOAD = Terminate, deps: [NET],  task: upload_worker;
}

let sub = Supervisor::new(&UPLOAD_GRAPH);
loop {
    state = match state {
        State::Menu => menu(&mut ctx).await,
        State::Upload => {
            WIFI_HW.provide(build_wifi(&mut ctx));   // rebuilt per entry
            sub.start(spawner).await?;               // WIFI -> NET -> UPLOAD, in order
            let next = upload_screen(&mut ctx).await; // state machine stays in charge
            sub.teardown().await?;                   // UPLOAD -> NET -> WIFI, reverse
            next
        }
        // ...
    };
}
```

`start()` is the universal quiescent-to-running op, so mixed-mode sub-graphs cycle
correctly: each node is reset per cycle (re-entry starts clean), running and detached
nodes are skipped (idempotent; a detached instance survived the teardown), and a
`Pause` instance parked by the previous `teardown()` is **resumed in place** rather
than double-spawned (spawned once, resumed every re-entry). `teardown()` awaits every
ack, so re-entering the state cannot race the previous instances — for the canonical
ack-by-returning bodies the previous task has fully exited (and freed its `TaskPool`
slot) before `teardown()` returns, so the default `pool_size` of 1 suffices for
cycling; `consume` slots make "rebuild the radio each entry" fail-closed instead of
stale-reuse.

**One-graph variant** *(feature `control`)*: declare the subtree `Terminate` +
`disabled` in the **main** graph and drive it as a dependency cascade — `Activate` on
the **leaf** pulls its transitive deps up in topo order (skipping already-running
ones), `Deactivate` on the **root** tears its transitive dependents down in reverse.
Prefer this over a separate graph when the subtree **depends on always-on nodes**
(graphs are closed worlds — there are no cross-graph dep edges), when the subtree
should ride the system-wide sleep/wake lifecycle (one `teardown()` covers it, and the
`disabled` latch keeps it down across the wake's `respawn_terminate()`), or when the
phase must be drivable from anywhere via `request_control` through the shared mailbox
(e.g. the supervisor lives inside a `run()` driver task):

```rust,ignore
State::Upload => {
    WIFI_HW.provide(build_wifi(&mut ctx));
    sup.activate(&UPLOAD, spawner).await;            // WIFI -> NET -> UPLOAD
    let next = upload_screen(&mut ctx).await;
    sup.deactivate(&WIFI).await?;                    // UPLOAD -> NET -> WIFI
    next
}
```

(`activate`/`deactivate` are the cascading, `disabled`-latching verbs — contrast the
single-node, no-cascade `start_node`/`stop_node`. `apply_control` is the same pair
routed through the `request_control` mailbox, for code that doesn't hold the
supervisor.)

Either way the supervisor is a library here, not the owner of bring-up.

Node and pool names below are invented; swap in your own worker fns. They use `task:`
throughout (the preferred form — plain async fns, no `#[embassy_executor::task]`);
substitute `spawn:` in the same position for any of the four cases in
[`spawn:` vs `task:`](#spawn-vs-task--which-to-use). Nodes shown without either are
**parked** on purpose: the application spawns them itself, so those workers do keep the
attribute.

### Simple dependency chain

```rust,ignore
supervisor_graph! {
    node SENSOR   = Terminate, deps: [], task: sensor_worker;
    node REPORTER = Terminate, deps: [SENSOR], task: reporter_worker;
}
```

`REPORTER` is brought up only after `SENSOR`. The topological order is computed at compile
time — a cycle or an unknown dep name is a compile error.

### Generic worker over N driver types (`task:`)

```rust,ignore
// ONE generic worker — a plain async fn, not a #[embassy_executor::task]:
async fn poll_sensor<D: Sensor>(node: &'static TaskNode, dev: D) {
    while let Ok(v) = node.run_cancellable_acked(dev.sample()).await {
        publish(v);
    }
}

supervisor_graph! {
    node BUS = Terminate, deps: [], task: bus_worker;
    // One node per concrete driver; the macro stamps a monomorphized shell each:
    node BME = Terminate, deps: [BUS], task: poll_sensor::<Bme280>(bme());
    node SHT = Terminate, deps: [BUS], task: poll_sensor(sht());  // inferred
}
```

Args (`bme()`, `sht()`) are evaluated inside each shell at first poll, on the
node's own executor.

### Provider node — async multi-output construction in the graph

One async bring-up often builds SEVERAL correlated driver objects (a cyw43 radio:
two runners + a `Control` + a `Stack` handle) that different nodes consume, and must
re-run every wake cycle. That builder becomes an ordinary **provider node** — no
special DSL, just the gate machinery pointed at runtime provisioning:

```rust,ignore
// The provider: builds and provide()s, holds NOTHING afterwards. Terminate
// mode makes respawn_terminate re-run the build each wake cycle.
async fn radio_hw(node: &'static TaskNode) {
    let (runner, control, stack) = build_radio().await;  // hundreds of ms
    RUNNER.provide(runner);     // consume slot: empty again after teardown
    CONTROL.provide(control);   // consume slot
    STACK.provide(stack);       // shared slot: fanned out, stays filled
    node.wait_shutdown().await;
    node.ack_dropped();
}

supervisor_graph! {
    node RADIO_HW = Terminate, deps: [], task: radio_hw;
    // Consumers: deps order them after the provider, and slot_timeout covers
    // its build time (the 100 ms default assumes provided-before-start).
    node LINK = Terminate, deps: [RADIO_HW], task: link_worker, slot_timeout: 5000,
        resources: [RUNNER: local consume Runner];
    node CTRL = Terminate, deps: [RADIO_HW, LINK], task: ctrl_worker, slot_timeout: 5000,
        resources: [CONTROL: local consume Control, STACK: shared local Stack];
}
```

The lifecycle falls out of the existing rules: `start()` spawns `RADIO_HW` first
(topo order) and parks on the consumers' gates until it has provided; teardown drops
consumers first (reverse topo — `consume` values are dropped, `shared` handles just
die with their copies) and the provider last; `respawn_terminate` re-runs the
provider FIRST, so the consumers' gate waits rendezvous with the freshly built
values. A provider that dies before providing surfaces as `SpawnError::Busy` on its
consumers after their `slot_timeout` — fail-closed, never a stale reuse.

### Readiness rendezvous (`ready` dep marker)

A plain dep orders spawns; the `ready` marker (feature `readiness`) additionally
holds the dependent until the dep's task says it is actually serving:

```rust,ignore
supervisor_graph! {
    node NET  = Terminate, deps: [], task: net_worker;
    node HTTP = Terminate, deps: [NET ready], task: http_worker,
        slot_timeout: 10000;   // how long HTTP's spawn waits for NET's set_ready()
}

async fn net_worker(node: &'static TaskNode) {
    bring_link_up().await;                       // DHCP, registration, calibration…
    node.set_ready();                            // NOW dependents may spawn
    let _ = node.run_cancellable_acked(serve()).await;
    // (a link-loss handler would clear_ready() — status, not control: already-
    // running dependents keep running; future spawns and pool growth wait)
}
```

`set_ready()` latches until `clear_ready()` or the pre-spawn reset (a respawned
provider re-asserts for its new instance). The wait is bounded by the DEPENDENT's
`slot_timeout:` and fails the spawn with `SpawnError::Busy`, so a provider that never
becomes ready is a loud, retryable error — never a hang.

### Elastic worker pool with `DeferredShrink`

```rust,ignore
supervisor_graph! {
    node BROKER = Terminate, deps: [], task: broker_worker;
    pool WORKERS = [Terminate, OnDemand, OnDemand, OnDemand], deps: [BROKER],
        task: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: 1, max: 4;
}
```

Four member slots; `min: 1` is the always-on floor, growth up to `max: 4` under load.
`DeferredShrink` waits 4 s of idle surplus before shrinking so brief lulls don't thrash.
Requires the `pool` feature. `task:` on a pool emits ONE shell sized to the member count,
so there is no `pool_size = 4` constant to keep in sync with `max:`.

### Pause node holding a resource (parked, app-spawned)

```rust,ignore
supervisor_graph! {
    node SENSOR = Pause, deps: [];   // neither `task:` nor `spawn:` => parked node
    node READER = Terminate, deps: [SENSOR], task: reader_worker;
}

// main() spawns the sensor task itself, with the peripheral handle it owns:
spawner.spawn(sensor_task(&SENSOR, i2c).unwrap());
```

A `Pause` node acks a shutdown, then parks on `wait_resume()` — the I2C handle it holds is
never dropped. `resume_pausable()` thaws it in place after a wake.

### Control-started node (`disabled`)

```rust,ignore
supervisor_graph! {
    node NET     = Terminate, deps: [], task: net_worker;
    node UPDATER = Terminate, deps: [NET], task: updater_worker, disabled;
}
```

`start()` skips `UPDATER` at boot; it comes up only when runtime control targets it with
`request_control(&UPDATER, ControlOp::Activate)`. Use for on-demand subsystems (a firmware
updater, a debug server) that shouldn't run until explicitly asked for.

### Detached self-managed daemon

```rust,ignore
supervisor_graph! {
    node LOG_DRAIN = Terminate, deps: [], task: log_drain_worker;
}

// Plain async fn — `task:` stamps the #[embassy_executor::task] shell:
async fn log_drain_worker(node: &'static embassy_supervisor::TaskNode) {
    node.set_detached(true); // full hands-off from here on
    loop { /* drain forever, self-managed */ }
}
```

After `set_detached(true)` the supervisor never drives the node again — teardown, control
cascades, `stop_node`, respawn and pause-resume all skip it. The graph stays the single place
it's declared and ordered; management stops after the first spawn.

### Interrupt-priority executor tier

```rust,ignore
supervisor_graph! {
    executor HIGH;   // runtime-filled SendSpawner slot (an interrupt-priority tier)
    node SAMPLER = Terminate, deps: [], executor: HIGH, task: sampler_worker;
    node LOGGER  = Terminate, deps: [SAMPLER], task: logger_worker;
}

// app side, before `sup.start(...)` (embassy-rp shown; any HAL works):
static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();
interrupt::SWI_IRQ_0.set_priority(Priority::P2);
HIGH.set(EXECUTOR_HIGH.start(interrupt::SWI_IRQ_0));
```

`SAMPLER` runs at raised priority while `LOGGER` stays on the thread executor — yet the
dependency between them is still honored. `sampler_worker`'s future must be `Send`; if the
slot is never filled, `start()` fails with `SpawnError::Busy` after a bounded wait. A
`task:` extra is evaluated inside the shell, i.e. on the raised-priority tier at its first
poll — switch that node to `spawn:` when an argument must instead be snapshotted on the
supervisor's executor at the moment of the spawn (case 4 of
[`spawn:` vs `task:`](#spawn-vs-task--which-to-use)).

### Second-core pool

```rust,ignore
supervisor_graph! {
    executor CORE1;
    pool CRUNCHERS = [OnDemand, OnDemand], deps: [], executor: CORE1,
        task: cruncher_worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(2)),
        min: 0, max: 2;
}
```

The pool members run on core 1's executor while core 0's supervisor scales them. Core 1's
entry publishes its spawner (`CORE1.set(sp.make_send())` inside `executor.run`); `start()`
and `start_node` await the slot, so a late-booting core is a rendezvous, not a race.
`min: 0` lets the pool scale fully down when idle.

### Node depending on a pool

```rust,ignore
supervisor_graph! {
    pool WORKERS = [Terminate, OnDemand], deps: [],
        task: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
    node DISPATCHER = Terminate, deps: [WORKERS], task: dispatcher_worker;
}
```

A dep on a pool name resolves to the pool's **floor member**, so `deps: [WORKERS]` means
"start `DISPATCHER` once the pool floor is up".

### Run-once check, ordered last

```rust,ignore
supervisor_graph! {
    node NET = Terminate, deps: [], task: net_worker;
    pool WORKERS = [Terminate, OnDemand], deps: [NET],
        task: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
    node READY_PROBE = Terminate, deps: [WORKERS], task: ready_probe_worker;
}

async fn ready_probe_worker(node: &'static embassy_supervisor::TaskNode) {
    node.set_detached(true);
    // everything above is up now; do a one-shot post-boot self-check, then return
}
```

`deps: [WORKERS]` on a leaf node makes it the last thing brought up. Its exit is observed
(the `task:` shell records it via `mark_exited()`, so teardown skips it either way);
detaching is still what makes it run once **ever** — without it, `respawn_terminate` on
the next wake cycle would re-run the completed node.

### Composite: sensor tier + parked diagnostics + power coordinator

```rust,ignore
supervisor_graph! {
    executor HIGH;                    // interrupt-priority tier

    node SENSOR   = Terminate, deps: [], executor: HIGH, task: sensor_worker;
    node NET      = Terminate, deps: [], task: net_worker;
    node UPLOADER = Terminate, deps: [NET, SENSOR], task: uploader_worker;
    node STATS    = Pause, deps: [], task: stats_worker;   // parked through sleep
    node POWER    = Terminate, deps: [];  // parked: main spawns it with the Spawner
}

static SUP: Supervisor<5> = Supervisor::new(&GRAPH);

// A parked node (neither `task:` nor `spawn:`): main spawns it by hand because it
// needs a value only main has — here the `Spawner` that `respawn_terminate` takes:
//     spawner.spawn(power_task(&POWER, spawner).unwrap());
#[embassy_executor::task]
async fn power_task(node: &'static embassy_supervisor::TaskNode, spawner: Spawner) {
    node.set_detached(true); // survives the teardown it is about to drive
    loop {
        wait_for_idle().await;
        SUP.teardown().await;                       // quiesce the graph; POWER is skipped
        enter_low_power().await;                    // Pause nodes stay parked
        SUP.resume_pausable();                      // thaw the parked diagnostics
        SUP.respawn_terminate(spawner).await.ok();  // respawn the stateless services
    }
}
```

The common shapes combined: a latency-critical node on an interrupt tier, a `Pause`
diagnostics node that keeps its state across the sleep, and a detached coordinator that
drives the whole sleep/wake cycle itself — because it's detached, its own `teardown()` and
`respawn_terminate()` calls skip it.

## Elastic pools

`ElasticPool` scales single-instance members between `min` and `max` running instances.
Workers report load (`mark_busy`/`mark_idle` + `request_scale`); the supervisor's
`run_pools(spawner)` future — `select`ed against `wait_control()` in the driver loop — wakes
on each scale request (it never polls), asks each pool's `ScalingPolicy` for a `PoolAction`,
and starts/stops one member accordingly. A member is never grown while one of its declared
dependencies is down (or, with `readiness`, while a `ready`-marked dep is un-ready).

**The whole driver is one call** when you don't need extra select arms:
`sup.run(spawner).await` = `start()` + drive pools and control forever, returning a
`RunError` only on error (bring-up spawn failure, or a missed shutdown ack) — every arm
an app-level escalation, typically `panic!` into a hardware-watchdog reset. Apps that
select their own wake sources into the loop keep writing
`select(sup.run_pools(spawner), wait_control())` + `apply_control` by hand.

The built-in `DeferredShrink` policy grows immediately when saturated (no idle member, below
`max`) and shrinks only after an idle surplus has persisted for a configurable cooldown —
responsive up, lazy down. One idle spare is the stable dead-band, so a single spare never
flaps. Swap in your own policy by implementing `ScalingPolicy` (a sync, allocation-free
decision fn).

## Multi-executor tiers and multi-core

The `executor` mechanism is one story at two scales: an `InterruptExecutor` tier on the same
core, or a second core running its **own** executor. Either way, tasks never migrate and the
graph is the single source of *placement*.

```rust,ignore
supervisor_graph! {
    executor CORE1;
    node BENCH = Terminate, deps: [], executor: CORE1, task: bench_worker, disabled;
}

// core 1 publishes its spawner as it boots (embassy-rp shown; any HAL works):
spawn_core1(p.CORE1, &mut CORE1_STACK, || {
    EXECUTOR1.run(|sp| CORE1.set(sp.make_send()))
});

// bring-up rendezvouses with that asynchronous publish as part of `start` itself
// (bounded wait per `executor:` node, then `SpawnError::Busy`):
sup.start(spawner).await?;
```

Everything the supervisor does is already cross-core sound (atomics + critical-section
primitives): teardown awaits acks from the other core, `apply_control` starts/stops
remote nodes, and a whole `pool` can carry `executor: CORE1` — an elastic worker pool
on core 1, scaled by core 0's supervisor. With `trace`, the other core's executor shows
up as its own line in the stats; register `trace::set_core_id_fn` (one line, e.g. read
`SIO.CPUID` on RP2350) to keep `trace-nested` exact per core. Explicit non-goals: task
migration and work stealing (futures aren't `Send` across most HALs — each node lives
where the graph puts it).

## Composing graphs across crates

`supervisor_graph!` is one closed invocation — but it does not have to be one closed
*file*. A module (or a whole crate) declares its slice of the graph as a **fragment**,
and one compose site assembles them:

```rust,ignore
// net.rs (or a separate crate)
embassy_supervisor::supervisor_fragment! {
    name: NET_FRAG;
    node NET = Terminate, deps: [], task: $crate::net::net_task,
        resources: [USB_DEV: Peri<'static, USB>];
}

// main.rs — the one compose site per binary
embassy_supervisor::compose_graph! {
    fragments: [NET_FRAG, ::http_stack::HTTP_FRAG],
    graph: {
        node APP = Terminate, deps: [NET], task: app_worker;  // cross-fragment dep
    }
}
```

A fragment emits a `#[macro_export]` relay macro that forwards its items — verbatim,
with their spans — into the compose site's single `supervisor_graph!` expansion. So
**every compile-time pass still sees the whole graph**: cross-fragment deps resolve by
name in either direction (forward references included), duplicate names and shared-slot
shape mismatches error with the owning fragment named, the topological order and the
256-node cap span everything. All statics (nodes, slots, `GRAPH`) land at the compose
site.

Rules and caveats:

- **Paths**: a fragment references its own workers/types via `$crate::…` (resolves to
  the fragment's crate at any compose site) or fully-qualified `::crate_name::…`. A bare
  `crate::…` resolves at the *compose* crate — a bug unless they are the same crate. No
  `$` other than `$crate` is permitted (validated).
- **`#[cfg(...)]` inside a fragment is evaluated against the COMPOSE crate's features**
  (the tokens expand there). A fragment crate that wants feature-dependent shapes
  exports differently-named fragment variants instead.
- One compose site per binary (it emits the graph statics and, under `trace-hooks`,
  the hook symbols); fragment names are crate-root macros — prefix them.
- Fragment item syntax is validated at the fragment site with its own spans; only
  name *resolution* waits for the compose site.

### Multiple graphs per binary

`name: IDENT;` as a graph's first item (also `compose_graph! { name: X, … }`) renames
the emitted static and suffixes every generated helper, so several supervisors coexist
— e.g. an always-on primary graph plus a [subordinate sub-graph]
(#subordinate-sub-graph-under-an-app-state-machine) the app cycles. Rules:

- **The unnamed graph is the primary**: under `trace-hooks` only it emits the
  once-per-binary `_embassy_trace_*` symbols; named graphs are secondary (their nodes
  still resolve in the trace recorders — each `start()` registers its graph, up to
  `trace::MAX_GRAPHS`).
- **The control mailbox and scale signal are shared.** Run ONE driver (one `run()` or
  one `run_pools`/`wait_control` loop) and apply each command to every supervisor in
  turn — a command naming a node outside a supervisor's graph is a safe no-op. Two
  independent driver loops would race each other for commands.
- Only the graph static itself is renamed (plus internal generated helpers); node
  and resource-slot statics keep exactly their declared names — `WIFI` in a named
  graph is still `WIFI`. Two graphs reusing a node name in one module is therefore an
  ordinary duplicate-static error; the 256-node cap is per graph.

## Observability

*(feature family `trace` — all opt-in)*

embassy-executor ships raw `_embassy_trace_*` instrumentation hooks that identify tasks only
by an opaque `u32`. The `trace` feature makes the supervisor their batteries-included
consumer: the generated spawn glue captures each `SpawnToken`'s id into its node, so every
executor poll is attributed to a *named* node — correctly across respawns.

- **Per node**: accumulated poll time (`exec_ticks`), poll count, and the longest single
  poll ever (`max_poll_ticks`) — the "never yields" watermark that names a task that hogged
  its executor, even after the fact.
- **Per executor**: a full time decomposition via `trace::executor_stats` — idle, in-poll
  (every task poll, supervised or not), and by subtraction the **executor overhead**
  (scheduler bookkeeping + hook cost + ISRs between polls) and the unsupervised-task
  share — plus poll/pass counters and the in-flight poll (`trace::current_task` /
  `trace::stalled_task(executor, threshold)` for live blocked-task detection from a
  context that can still run).
- Counters are wrapping `u32` ticks: sample twice, `wrapping_sub`, divide. The in-repo
  firmware's README covers how to read the numbers in practice (CPU%, busy% vs overhead,
  polls-per-pass as a wake-storm tell).

The split across the family: `trace` is recorders only; `trace-hooks` additionally emits the
seven hook symbol definitions at the graph declaration site (exactly one set may exist per
binary — define your own hooks and forward to the `trace::on_*` recorders if you need
custom ones); `metadata-names` stamps node names into task Metadata for external tooling
(SystemView, debuggers); `trace-names` is shorthand for `trace` + `metadata-names`;
`trace-nested` makes accounting preemption-exact — a nested higher-tier poll credits its
time back to the window it interrupted (register `trace::set_core_id_fn` on multi-core for
one preemption stack per core).

`metadata-names` is independent of `trace`: it pulls only `embassy-executor/metadata-name`,
not `embassy-executor/trace`, so it emits **no** `_embassy_trace_*` hook symbols and links
cleanly on its own. That makes it the piece you want for a pure external tracer: enable
`metadata-names` alongside embassy's own `rtos-trace` feature (which also pulls
`metadata-name`) and SystemView shows your graph's node names — with none of the supervisor's
recorder overhead and no hook-symbol requirement. Enabling `trace`/`trace-names` instead
brings the recorders back and, as ever, requires the hook symbols (`trace-hooks` or your own).

Limitations: accounting is preemption-naive without `trace-nested`; hardware-ISR time is
invisible either way; executor busy% exceeds the per-node sum by a per-poll accounting gap
(`ExecutorStats` measures it as `busy − in-poll`); at most 4 executors are tracked. Parked /
closure-spawned nodes register with one call: `TaskNode::adopt(&token)`. The hook API is an
executor implementation detail — this feature tracks the executor minor version the crate
already pins.

## Cargo features

| feature   | default | what it adds |
|-----------|:-------:|--------------|
| `control` |    ✓    | runtime control plane (`ControlOp`, `request_control`, `apply_control`) |
| `pool`    |    ✓    | elastic worker pools (`ElasticPool`, `run_pools`, `GRAPH.pools`) |
| `macros`  |    ✓    | the `supervisor_graph!` graph-declaration macro |
| `local-resources` | | permit the `local` resource kind — ⚠ opt-in to the macro emitting a documented `unsafe impl Sync` (single-core contract) |
| `readiness` | | task-asserted readiness: `set_ready`/`wait_ready`/`clear_ready` + the `ready` dep marker (bring-up + pool-growth gating) |
| `liveness` | | per-node heartbeat: `beat()` stamps the embassy-time clock, `ticks_since_beat() -> u32` (embassy-time ticks), `is_stale(max_age)` — alive-but-wedged detection without `trace`. A fresh spawn counts as a beat, so a node is never instantly stale |
| `heap-state` | | `state: Type = expr` per-activation boxed state, reclaimed on task exit — ⚠ opt-in: emits the ~6-line fallible-boxing `unsafe` helper into your crate; needs a `#[global_allocator]` |
| `defmt`   |         | route the supervisor's logs through `defmt` (otherwise the log macros are no-ops) |
| `trace`   |         | trace-hook observability: per-node CPU time / poll counts / max-poll watermark, executor idle time, stall detection |
| `trace-hooks` |     | batteries-included: the graph declaration also defines the `_embassy_trace_*` hook symbols (implies `trace`) |
| `metadata-names` |  | stamp node names into task Metadata for external tooling (rtos-trace/SystemView); independent of `trace` — no hook symbols |
| `trace-names` |     | shorthand for `trace` + `metadata-names` |
| `trace-nested` |    | preemption-exact accounting: nested higher-tier polls are credited back to the window they interrupt (implies `trace`) |

`default-features = false` gives a minimal core that only does dependency-ordered
bring-up/teardown — dropping the control plane and pools trims flash and a couple of statics.

## Testing on the host

The crate is HAL-free, so graphs run on a desktop for tests: embassy-executor's
`platform-std` + `executor-thread` features give a std `Executor` to run on a thread,
and `embassy-time`'s `mock-driver` provides the clock (also enable
`critical-section/std`). The whole harness is ~15 lines:

```rust,ignore
#[embassy_executor::task]
async fn driver(spawner: embassy_executor::Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(spawner).await.expect("bring-up");
    // ... assertions, teardown/start cycles ...
    DONE.store(true, Ordering::Release);
}

fn main() {
    let clock = embassy_time::MockDriver::get();
    std::thread::spawn(|| {
        let ex: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        ex.run(|spawner| spawner.spawn(driver(spawner).unwrap()));
    });
    while !DONE.load(Ordering::Acquire) {
        // Advance ONLY to observe a timeout (ShutdownTimeout / gate Busy) or
        // liveness staleness — cross-thread advance is sound.
        // clock.advance(embassy_time::Duration::from_millis(500));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
```

A frozen mock clock is fine on the happy paths — every wait resolves by signal (acks,
slot fills, readiness), and the internal timeouts exist only to convert a *failure*
into an error, so advance the clock only when a test wants to observe
`ShutdownTimeout`, a gate `Busy`, or `is_stale` flipping (the liveness clock IS
embassy-time, so the mock drives it too). `heap-state` needs no `#[global_allocator]`
on the host — std provides one. The crate's own integration tests are all built this
way.

## no_std / MSRV

`#![no_std]` and `#![forbid(unsafe_code)]`. Requires Rust 1.85+ (edition 2024). The embassy
dependencies are pre-1.0 (`embassy-executor` 0.10, `embassy-sync` 0.8, `embassy-time` 0.5), so a
consuming application must use compatible embassy minor versions.

## Full example

The [`firmware`](https://github.com/cedrivard/embassy-supervisor/tree/main/firmware) crate in the
repository is a complete working application on an RP2350 — networking, an HTTP control plane, an
elastic worker pool, multi-executor tiers on both cores, trace observability, and OTA firmware
update — all driven by this supervisor.

## Earlier release highlights

Condensed feature tours of past releases; the CHANGELOG is the authoritative history.

### 0.4.0

Ships with `embassy-supervisor-macros` 0.5.0 .

The release where the graph stopped being one flat literal per binary — fragments
compose it across crates, `name:` gives a binary several of them, and
`start()`/`teardown()` became a repeatable cycle — on a lifecycle core that now
**observes** what it supervises: a task's own completion is recorded, readiness is
asserted rather than assumed, control delivery is guaranteed, and every shutdown
outcome is a value the application can act on.

- **Every outcome is a value** *(breaking)*. `stop_node`, `teardown` and
  `apply_control` return `Result<(), ShutdownTimeout>` naming the offending node
  (`run_pools` returns `ShutdownTimeout`), so a missed ack becomes an escalation the
  application owns — retry, log, reset — instead of a decision made inside the
  library; `teardown` stops the cascade at the first timeout so a still-live
  dependent never has its dependencies pulled out from under it, and
  `teardown_continue()` is the deliberate "hardware reset next" counterpart.
  `request_control` is `async` and awaits mailbox capacity, with
  `try_request_control` (`Err(ControlQueueFull)`) for ISRs and callbacks — a command
  is now either delivered or refused, never silently lost. See
  [Migration](#migration).
- **Observed completion.** `mark_exited()` / `has_exited()`: a body that returns is
  recorded as completed and its handshake acked, so a run-once task reads as
  *finished* and a control `Activate` can respawn it. That flag is also what makes a
  *parked* `Pause` instance distinguishable from an exited one, which is why
  `start()` is now the universal quiescent-to-running op — reset each node, skip
  running and detached ones, resume a parked one in place — making
  `start()`/`teardown()` a repeatable cycle for a
  [subordinate sub-graph](#subordinate-sub-graph-under-an-app-state-machine).
- **Composable graphs.** `supervisor_fragment! { name: X; <items> }` lets a module or
  a whole crate declare its slice of the graph and `compose_graph!` assembles them
  into ONE expansion — cross-fragment deps resolve by name in either direction, every
  compile-time pass runs over the whole composed graph, and errors are attributed to
  the owning fragment. See [Composing graphs](#composing-graphs-across-crates).
- **Named multi-graphs.** `name: IDENT;` as a graph's first item renames the emitted
  static and suffixes every generated helper, so several supervisors coexist per
  binary. The unnamed graph stays the primary (only it emits the `trace-hooks`
  symbols) and the control mailbox is shared — run ONE driver and apply each command
  to every supervisor. See [Multiple graphs per binary](#multiple-graphs-per-binary).
- **`readiness` and `liveness`** *(both off by default)*. `deps: [NET ready]` holds a
  dependent's spawn until the provider calls `set_ready()` — a real rendezvous on
  "actually serving" (DHCP bound, registration done) rather than "spawned", bounded
  by the dependent's `slot_timeout` and then a `SpawnError::Busy` naming the
  not-ready dep. `beat()` + `is_stale(max_age)` catch the alive-but-wedged task an
  ack-based check cannot see. One AtomicBool + Signal + slice, and one AtomicU32, per
  node. See [Readiness rendezvous](#readiness-rendezvous-ready-dep-marker).
- **`heap-state`** *(off by default)*. `state: Type = init_expr` on `task:` nodes and
  pool members: fallibly boxed per activation (alloc failure = `SpawnError::Busy`,
  retryable), lent to the worker as `&mut Type`, dropped on exit before restores —
  every activation allocates fresh, net zero across respawns, while task STORAGE
  stays static by soundness. See [Heap and the graph](#heap-and-the-graph).
- **Pools grew up.** Take-kind `resources:` entries become per-member slot arrays
  (member `I` owns element `I` exclusively; a lend value survives shrink and regrow
  on the same index), `min:`/`max:` accept const-evaluable expressions guarded by
  const asserts, and `ElasticPool::member_index(node)` indexes per-member app state.
- Also: [`exit: Type`](#exit--typed-exit-values) — the worker's return value lands in
  a generated `<NODE>_EXIT` slot just before the completion is recorded;
  `run_cancellable` / `run_cancellable_acked` as combinators; `resume_node()`, and
  `activate`/`deactivate` now public; and `Supervisor::run(spawner)`, which is
  `start()` plus the pool-scaling and control loop in one call.

### 0.3.3

Ships with `embassy-supervisor-macros` 0.4.0 .

Three `resources:` kind markers — **`consume`**, **`shared`**, **`local`** — plus
per-node **`slot_timeout:`** and the **provider-node** pattern: hardware init is now
fully graph-managed across every power-state transition (cold boot, dormant wake,
deep-sleep wake), and the hand-rolled statics, `unsafe` accessors, and panic-prone
init getters they used to require are gone.

- **`consume`: drop-at-teardown / rebuild-per-cycle resources.** The worker owns the value
  outright, so dropping it at teardown is part of the contract (a driver whose `Drop`
  releases pins and DMA channels), and the slot stays empty afterwards — a respawn
  fail-closes with `SpawnError::Busy` until the app `provide()`s a fresh instance, instead of
  silently reusing a driver that went stale across a power cycle.
- **`local`: `!Send` driver handles on a single core.** `RefCell`-/`NoopRawMutex`-based
  handles — driver control handles, network-stack runners — can now ride `resources:`: the
  entry's slot is a graph-site type without the `T: Send` bound (it carries a documented
  `unsafe impl Sync` in *your* crate; single-core contract, and `local` + `executor:` is a
  compile error). Because that injects unsafe code, `local` requires the non-default
  `local-resources` feature (since 0.3.4).
- **`shared`: one `Copy` handle fanned out to many consumers.** Several nodes — and whole
  `task:` pools — declare the SAME slot name (a network-stack handle, a `&'static`
  shared-bus ref); each spawn copies the value out non-destructively and the slot stays
  filled. This replaces the panicking-accessor pattern (an `is-it-initialized-yet` getter
  as a `task:` extra): a missing handle is now a gate-awaited, fail-closed
  `SpawnError::Busy` instead of a first-poll panic.
- **`slot_timeout:` + provider nodes.** The pre-spawn slot/gate wait is per-node tunable
  (`slot_timeout: 5000`, `TaskNode::with_slot_timeout`), which makes an async hardware
  builder an ordinary graph node: build, `provide()`, park; consumers rendezvous on
  their gates — `start()` and every `respawn_terminate()` alike (the provider re-runs
  first, in topo order). See
  [Provider node](#provider-node--async-multi-output-construction-in-the-graph).
- Also: per-entry `#[cfg(...)]` on `resources:` entries, and generated shells silence the
  `unreachable_code` warning for `-> !` workers with restore-kind resources.

Combined, they make a whole radio bring-up fully graph-managed — a provider node builds
the driver objects and `provide()`s them (`RUNNER: local consume …` for the owned `!Send`
event loop, `STACK: shared local …` for the fanned-out handle), `start()` rendezvouses,
teardown drops them, and the next wake cycle rebuilds and re-provides. See
[Resource kinds](#resource-kinds-local-consume-and-shared).

### 0.3.2

Ships with `embassy-supervisor-macros` 0.3.1 .

New **`metadata-names`** feature: stamp node names into task `Metadata` independently of the
`trace` recorders (no `_embassy_trace_*` symbols). Use it to:

- **See graph node names in SystemView / a debugger** while profiling on a J-Link — enable it
  next to embassy's `rtos-trace` and the timeline reads `NET`, `HTTP`, `OTA` instead of opaque
  task ids, with none of the supervisor's per-poll recorder overhead.
- **Get readable task names in a RAM dump or `defmt` task view** on a shipping build where you
  don't want the trace layer's cost but still want to tell tasks apart in a crash log.

`trace-names` is now shorthand for `trace` + `metadata-names`, so the full trace layer (with
names) is unchanged; the name stamp is just usable on its own now.

### 0.3.1

Ships with `embassy-supervisor-macros` 0.3.0 .

- **`task:` — generated shells.** Declare a **plain async worker fn** — possibly generic —
  and the macro stamps its concrete `#[embassy_executor::task]` shell per declaration; a
  `task:` pool's shell is auto-sized to the member count. No attribute boilerplate, and
  the graph becomes the single place task plumbing lives (see
  [`spawn:` vs `task:`](#spawn-vs-task--which-to-use) — `task:` is now the preferred form).
- **Safe resource threading.** `resources: [NAME: Type, ..]` on a `task:` node emits a
  `ResourceSlot<Type>` static: `main` **moves** the peripheral in with `provide()`
  (consuming the `Peripherals` field — compile-time exclusive ownership, no `steal()`
  inside tasks), the glue `take()`s it before each (re)spawn (unprovided → `SpawnError::Busy`
  out of `start()`, fail-closed), the worker receives `&mut Type`, and the shell
  `restore()`s it on exit so a respawn re-takes the *same instance*. See
  [`resources:`](#resources--safe-resource-threading).
- **`ResourceSlot` / `ResourceGate` API.** The slot type behind `resources:` is public and
  usable by hand — e.g. share one slot between the generated glue and a manual
  `take()`/`restore()` borrower elsewhere in the app; `TaskNode::with_resources` makes
  bring-up await provisioning (bounded, then `SpawnError::Busy`).
- **Pool structural consts.** Each `pool` also emits `NAME_MIN` / `NAME_MAX` /
  `NAME_MEMBERS` (`usize`) for downstream const-context sizing
  (`const SOCKET_BUDGET: usize = HTTP_MAX + 1;`) — a `const` can't read them off the
  member `static` array.

Measured on the demo firmware (RP2350, release + fat LTO): the whole feature set costs
~1.5 KiB flash and a few dozen bytes of RAM; the generated shells add **zero**
steady-state stack — a threaded resource travels inside the task's future.

## Migration

### 0.4.0 → 0.4.1

Ships with `embassy-supervisor-macros` 0.6.0 (pinned by exact version — no action
needed). Purely additive at run time, with one source-level catch: `exit:` declared on
a worker that can never return is now [a compile error](#limits-and-compile-time-validation)
instead of a slot nothing ever filled. If a graph hits it, drop the `exit:` clause —
that worker never produced a value in the first place.

### 0.3 → 0.4

Ships with `embassy-supervisor-macros` 0.5.0 (pinned by exact version — no action needed).
Everything else in 0.4.0 is additive; three edits cover the breaking surface, and the
compiler finds all three:

| 0.3.x | 0.4.0 |
|---|---|
| `request_control(cmd)` (sync, silently dropped on a full mailbox) | `request_control(cmd).await` (awaits capacity), or `try_request_control(cmd)` → `Err(ControlQueueFull)` in a sync context (ISR, callback) |
| `sup.stop_node(&N).await` / `teardown()` / `apply_control(..)` panicked on a missed ack | they return `Result<(), ShutdownTimeout>` (`.node.name` names the offender); `.unwrap()` restores the old behavior |
| `sup.run_pools(spawner).await` never returned | returns `ShutdownTimeout` (only on a shrink whose member missed its ack) |
| a hand-written `spawn:` task calling `node.ack_dropped()` **on exit** | call `node.mark_exited()` there instead (acks *and* records completion, so the node stops reading as running); `ack_dropped()` stays correct for a `Pause` node's park |

`teardown()` now aborts at the first missed ack instead of stopping a wedged node's
dependencies under it; `teardown_continue()` is the previous best-effort sweep, for the
"hardware reset next" path. Generated `task:` shells call `mark_exited()` themselves, so a
`task:`-only graph needs no task-side change at all.

Worth adopting, though nothing forces it: `sup.run(spawner)` replaces the hand-written
`start` + `select(run_pools, wait_control)` driver, and `node.run_cancellable_acked(fut)`
replaces the hand-written select against `wait_shutdown()`.

### 0.2 → 0.3

Bring-up went `async`; the callers are already async tasks, so the change is mechanical:

| 0.2.x | 0.3.0 |
|---|---|
| `sup.start(spawner)?` | `sup.start(spawner).await?` |
| `sup.start_node(&N, spawner)?` | `sup.start_node(&N, spawner).await?` |
| `sup.respawn_terminate(spawner)?` | `sup.respawn_terminate(spawner).await?` |
| explicit `SLOT.ready().await` before `start()` | no longer needed — `start` awaits each `executor:` node's slot itself |

### 0.1 → 0.2

| 0.1.x | 0.2.0 |
|---|---|
| `task_graph! { &A, &B }` | `supervisor_graph! { node A = ...; node B = ...; }` |
| `Supervisor::new(&ALL_NODES, &DEPS, ORDER)` | `Supervisor::new(&GRAPH)` |
| `.with_pools(POOLS)` | gone — pools ride in `GRAPH` |
| `NODE_COUNT` | `GRAPH.nodes.len()` |

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
