# embassy-supervisor

[![crates.io](https://img.shields.io/crates/v/embassy-supervisor.svg)](https://crates.io/crates/embassy-supervisor)
[![docs.rs](https://docs.rs/embassy-supervisor/badge.svg)](https://docs.rs/embassy-supervisor)
[![docs](https://img.shields.io/badge/docs-embassy--supervisor.github.io-blue)](https://embassy-supervisor.github.io/)


**Run-time supervision for embassy firmware**: a dependency-ordered task-lifecycle
supervisor and a declared-and-verified dataflow layer, for the
[embassy](https://embassy.dev) async embedded framework. The graph declares *when
each task runs* (bring-up order, modes, pools, executor placement) and *what data
flows between them* (reads/writes declared, polled, or derived straight from the
code) — the first checked at compile time. **HAL-agnostic**,
`no_std`, no allocator, no board crates — it compiles for any embassy target. The only
third-party deps are pure-embassy crates (`embassy-executor`/`-sync`/`-time`/
`-futures`) and `portable-atomic`.

## Table of contents

- [What it is](#what-it-is)
- [The two tiers](#the-two-tiers)
- [The grammar, at a glance](#the-grammar-at-a-glance)
- [Quickstart](#quickstart)
- [The model](#the-model)
- [Lifecycle reference](#lifecycle-reference)
- [Writing supervised tasks (the TaskNode API)](#writing-supervised-tasks-the-tasknode-api)
- [The `supervisor_graph!` DSL](#the-supervisor_graph-dsl)
- [Dataflow supervision](#dataflow-supervision)
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
  shutdown with acked handshakes, running as a dep-ordered wave — a dependency
  keeps serving until its stopping dependents have acked, unordered nodes stop
  concurrently), `respawn_terminate()`
  (dependency-ordered re-spawn after a wake), `resume_pausable()` (thaw parked nodes); a
  missed shutdown ack comes back as a `NodeFault` naming the node and what went wrong,
  never a hang or a library panic.
- **Multi-executor placement** — `executor:` annotations route nodes onto interrupt-priority
  tiers; the graph is the single source of *where each task runs*.
- **Multi-core placement.** The same mechanism spans the second core: `start()` rendezvouses
  with the other core's asynchronous executor bring-up as part of the bring-up loop, and a whole
  elastic pool can live on core 1, scaled by core 0's supervisor.
- **Safe resource threading** — `resources:` annotations hand owned values to workers
  through `ResourceSlot`s: peripherals moved in from `main` (compile-time exclusive
  ownership — no `steal()`), or values another node builds at runtime and `provides:`.
  The consumer's spawn waits for the value; a lent value is restored on exit so a
  respawn re-takes the same instance.
- **Dataflow declaration** *(feature family `coupling`)* — `reads:`/`writes:`
  entries name the actual signal statics tasks exchange data through, so the graph can
  answer who produces a signal and who consumes it, and the diagram tool can draw it.
- **Dataflow-driven liveness** *(features `coupling-observe`, `dataflow`)* — a
  declared write can be the node's *sign of life*: polled from outside (`observed beat`,
  with no change to the task) or carried by the access (`beat`, via node verbs).
  `#[dataflow]` goes further and **derives** a task's read/write tables from its body at
  compile time, so the declaration cannot drift from the code.
- **Health surface** *(features `liveness`, `readiness`, `node-status`)* — per-node
  heartbeat + staleness, task-asserted readiness that gates dependents, and a one-line
  self-description; all cheap atomic reads for an app-owned monitor.
- **Observability** *(feature family `trace`)* — per-node CPU time, poll counts and stall
  detection by consuming embassy-executor's trace hooks, with node *names* attached.

The supervisor deliberately does **not** allocate, own a HAL, manage power states, or know what your
tasks do — it orchestrates their *lifecycle* and checks their *dataflow*, and leaves the rest to
you. It also does **not**
catch panics: a panicking task is not captured or restarted (panic capture is off the table in a
`forbid(unsafe_code)` no_std library — it would need unwinding or the app-global panic handler).
Pair the supervisor with a hardware watchdog for crashes, and the `liveness` heartbeat for
alive-but-wedged tasks.

## The two tiers

Every capability above is reachable from one of two positions; mixing them freely,
per task and even per signal, is the normal case.

**The supervisor tier — code that holds its node.** A task declared in the graph
receives its `TaskNode` as the first parameter, and the node is the whole protocol:
lifecycle (`run_cancellable_acked`, `ack_dropped`), health (`beat()`, `set_ready()`,
`report_status()`), and dataflow (`node.put`/`node.get` under `#[dataflow]`). The
more a body routes through its node, the more the graph can check — up to
`discover`, where the body *is* the declaration:

```rust,ignore
#[embassy_supervisor::dataflow]
async fn eskf_task(node: &'static TaskNode) {
    loop {
        let est = fuse(node.get(&crate::signals::IMU_DATA));
        node.put(&crate::signals::ESTIMATE, est);
        node.beat();
    }
}
// node ESKF = Terminate, deps: [IMU ready], task: eskf_task, discover;
```

**The agnostic tier — code that never learns the supervisor exists.** Adopt
supervision without touching the code being supervised:

- a [**`cancel` node**](#cancel--supervisor-unaware-workers) runs a plain
  `async fn` — no node parameter, no handshake; the generated shell owns the
  shutdown race and drops the worker's future in place on stop:

  ```rust,ignore
  async fn telemetry(uart: &mut Uart<'static, Async>) -> ! { loop { /* ... */ } }
  // node TELEM = Terminate, deps: [NET], task: telemetry, cancel,
  //     resources: [UART: Uart<'static, Async>];
  ```

- a node is also a **free-standing health handle**: any code that can see the
  static — an ISR, a callback, a driver — can call `NODE.beat()` or
  `NODE.report_status(..)`, and the same monitor and status endpoint see it like
  any task's;
- [**observed coupling**](#liveness-and-readiness-by-polling-feature-coupling-observe):
  `writes: [SIG observed beat]` is polled from outside — neither the signal
  crate nor the task body sees the supervisor;
- the **observe facade** ([`embassy-supervisor-observe`](https://crates.io/crates/embassy-supervisor-observe)):
  a signal library implements `Observable`/`Sink`/`Source` in a line each,
  without depending on the supervisor.

Rules of thumb: existing or third-party code goes in as-is through the agnostic
tier (`cancel` + `observed`); code written for this graph takes the node and gets
the exact forms (`beat`, `discover`, `ready_on_write`); a private signal
behind a setter stays private — the setter takes the caller's node and callers
[adopt it](#the-node-as-the-access-path-feature-dataflow) with
`dataflow: [..]`.

## The grammar, at a glance

The whole DSL is three item kinds — `node`, `pool`, `executor` — each one line, comma-
separated clauses, `;`-terminated. A useful graph is often just nodes, deps and task:

```text
supervisor_graph! {
    node NET  = Terminate, task: net_task;
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
        , resources: [R: Type, ..]      //   owned values handed over at spawn (kinds: local/shared/consume)
        , provides: [R, ..]             //   slots this task fills at runtime, cleared at its shutdown ack
        , exit: Type                    //   capture the worker's return value
        , state: Type = expr            //   per-activation boxed state, freed on exit
        , state: zeroed Type            //   same, allocated zero-filled (Type: Zeroable)
        , cancel                        //   shell owns the shutdown race; worker takes no node
        , reads: [crate::SIG, ..]       //   declared dataflow; entry marker: observed
        , writes: [crate::TRIP veto]    //   a contributor slot of a VetoGate (feature veto)
        , writes: [crate::SIG beat]     //     `beat` marks the heartbeat write
        , discover                      //   or: bind the tables the task fn's #[dataflow] derived
                                        //     (a list beside it may only add markers)
        , dataflow: [crate::setter]     //   adopt an accessor fn's #[dataflow] tables
        , beat_timeout: MS , ready_on_write //   every value-level clause (the timeouts, beats,
                                        //     ready_on_write, disabled, discover) takes a #[cfg(...)] gate
        , pool_size: N , executor: NAME , slot_timeout: MS , ack_timeout: MS , disabled;

    pool NAME = [Mode, ..],             // one Mode per member, floor first
        deps: [..], task: worker,
        resources: [..],                //   take kinds become per-member slot arrays
        policy: DeferredShrink::new(..),// scaling policy (required)
        min: EXPR, max: EXPR            //   required; any order, like every clause
        , cancel;                       //   same flag, applied to every member
}
```

Reading rules, all regular: node and pool clauses may appear **in any order**
(only the mode after `=` is positional — for a pool, the mode list); a repeated
clause is a compile error; every clause is **inline on
its item** (there are no block forms and no top-level `resources { }` section); a
pool always names its `policy:`; take-kind resource names are **globally unique**
(each is one static — only `shared` names repeat, by design); and `detached` is not a
mode but a runtime call (`TaskNode::set_detached`). Anything structurally wrong — a
dependency cycle, an unknown name, a duplicate — is a compile error with a message,
never a runtime surprise.

## Quickstart

```rust,ignore
use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph};

// Declare the graph once: `supervisor_graph!` generates the node `static`s and a
// single `GRAPH` bundling the node slots, dep table, compile-time order, and pools.
// Each `task:` names a plain async worker fn (the macro stamps its
// `#[embassy_executor::task]` shell); `app` depends on `net`.
supervisor_graph! {
    node NET = Terminate, task: net_task;
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
    // hardware-watchdog reset). One fault type with a `Display` that names the
    // node and the cause.
    panic!("supervisor: {}", sup.run(&spawner).await);
}
```

`run()` = `start()` + the driver loop; call the pieces yourself (`start`, then a
`select(run_pools, wait_control)` loop) when the driver must watch extra wake sources.
Bring-up is `async` because an `executor:` node first awaits its slot; a plain
single-executor graph resolves immediately — the `.await` costs nothing.

## The model

Three pieces, all `static`:

- **`TaskNode`** — one per managed task: a private handle of atomic flags + signals,
  plus a reference to its `NodeCfg` — the immutable half (name, mode, spawn fn, gates,
  budgets, coupling tables), a separate flash-resident `static` the macro emits beside
  the node, so the atomics don't drag the constant data into RAM. Read the declared
  side through methods (`name()`, `mode()`, `slot_timeout()`, ...). The *task side* of
  the protocol is a handful of
  node methods — see [Writing supervised tasks](#writing-supervised-tasks-the-tasknode-api).
- **`Graph<N, T>`** — the macro-emitted `GRAPH`: `nodes` (fixed `[Option<&TaskNode>; N]` — a
  `#[cfg]`-ed-out node keeps its slot as `None`), `topo` (the `Topology`: per-node
  dependency indices + the compile-time topological order, or the zero-sized `Flat`
  when the graph declares no `deps:` at all), and `pools` (with the `pool` feature).
  Read the edges through `deps_of(i)` / `dependents_of(i, ..)` / `order()`. The
  topology also carries the graph's structural **shape** bits (which modes, gates,
  markers and pools the graph contains, decided at expansion), so the lifecycle code
  serving a structure the graph lacks is compiled out rather than branched over.
- **`Supervisor<N, T>`** — construction-free orchestration over `&GRAPH` (`new` is
  `const`; the macro emits a `GRAPH_TOPOLOGY` alias for `T`, so
  `static SUP: Supervisor<5, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);` works; `N` =
  total graph slots, pool members included — the same `N` as `Graph<N, T>`), in three
  tiers: whole-graph, single-node, and cascading subsystem verbs.

The full verb surface, with signatures (every error type is `Debug` — `.unwrap()` /
`.expect()` work — and `defmt::Format` under the `defmt` feature):

| verb | signature |
|---|---|
| `start` | `async fn(&self, &Spawner) -> Result<(), NodeFault>` — quiescent → running, any state |
| `run` | `async fn(&self, &Spawner) -> NodeFault` — `start` + drive pools/control; returns only on error |
| `teardown` / `teardown_continue` | `async fn(&self) -> Result<(), NodeFault>` |
| `respawn_terminate` | `async fn(&self, &Spawner) -> Result<(), NodeFault>` — wake pair, with... |
| `resume_pausable` | `fn(&self)` — ...this (sync: parked tasks pick up immediately) |
| `start_node` | `async fn(&self, &'static TaskNode, &Spawner) -> Result<(), NodeFault>` |
| `stop_node` | `async fn(&self, &'static TaskNode) -> Result<(), NodeFault>` — awaits the ack |
| `resume_node` | `fn(&self, &'static TaskNode)` — sync, `Pause` nodes only |
| `activate` | `async fn(&self, &'static TaskNode, &Spawner)` — cascade; spawn errors deliberately swallowed |
| `deactivate` | `async fn(&self, &'static TaskNode) -> Result<(), NodeFault>` — cascade |
| `apply_control` | `async fn(&self, ControlCommand, &Spawner) -> Result<(), NodeFault>` |
| `run_pools` | `async fn(&self, &Spawner) -> NodeFault` — completes only on error |

Error provenance: every lifecycle failure is one `NodeFault { node, kind: FaultKind }`
— `ExecutorSlotEmpty`, `ResourceMissing`, `ReadyDepTimeout { dep }`, `Spawn(SpawnError)`
or `ShutdownTimeout` — with an unconditional `Display` that names the node and the
cause, so `{fault}` is enough for an escalation message. `Aborted` and
`ControlQueueFull` are the other crate types; `SpawnError` is re-used from
`embassy_executor` and appears only inside `FaultKind::Spawn`. All the guarantees here
are cross-thread (release/acquire atomics) — a host test's main thread reads them as
safely as another task.

The control mailbox (feature `control`) is two free functions and two small types:
`async fn request_control(&'static TaskNode, ControlOp)` (lossless — awaits mailbox
capacity), `fn try_request_control(..) -> Result<(), ControlQueueFull>` (sync
contexts), and `enum ControlOp` — `Activate`, `Deactivate`, and (feature `restart`) `Restart`;
the enum is `#[non_exhaustive]`. Higher-level verbs (start/stop/pause/resume) fold
onto the first two per the node's `Mode`.
All of these are importable from the crate root (`embassy_supervisor::try_request_control`,
`embassy_supervisor::ControlOp`, …); a `NodeFault`'s fields are
`pub node: &'static TaskNode` and `pub kind: FaultKind`.

activate walks up through dependencies; deactivate walks down through dependents. They are opposites, but they compose symmetrically over a subtree.

A deactivate call marks only the seed node as disabled. The seed is the target node, or the whole pool if the target is a pool member. Dependents are marked as collateral instead. Collateral blocks automatic bring-up just like disabled, but `activate` clears it once no disabled node remains anywhere in the dependent's transitive dependencies. Any Terminate or Pause dependents released this way restart in the same wave.

Overlapping deactivations compose cleanly. A node below two deactivated ancestors restarts on the second `activate`. A node deactivated directly keeps its latch through an ancestor's cycle. A manual `start_node` overrides the hold, but the override only clears at spawn entry, so a node that started while a hold was being latched still runs flagged and releasable instead of coming up silently unheld.

Released OnDemand pool members are not started immediately. They regrow through demand from the elastic policy or a gated read, and `activate` prompts the pool driver to re-check standing demand. A pool deactivated directly still needs its own Activate later. Targeting any member expands to the whole pool, since membership is part of the seed.

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
and by the three lifecycle-spanning flags (`disabled`, `collateral`, `detached`). Other docs link here.

**Missed acks are errors, not panics.** Every stop path awaits the target's ack with a
2 s timeout (per-node override: `ack_timeout:`); a node that misses it is returned as a
`NodeFault`
(`FaultKind::ShutdownTimeout`) naming the node —
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
| `deactivate` *(control)* | seed: disabled + stopped; transitive dependents: `collateral` + stopped, dependents first | disabled (or `collateral`) + stopped, parks; stays parked | disabled + stopped — the whole pool, atomically, when a member is the target; `collateral` as a dependent | re-disabled (idempotent) | **skipped** — never pulled into the cascade, even when targeted directly |
| `activate` *(control)* | enabled + started, after its transitive deps; a `collateral` dependent with no disabled dep left is released + restarted in the same wave | enabled + resumed in place | enabled/released only — the pool policy regrows it under load (the wave pokes the pool driver) | this is the flag it clears | **skipped** — not re-enabled, not restarted; its `deps:` are start-ordering only and are not expanded |
| `stop_node` | shutdown + ack | shutdown + ack, parks (**this is the single-node pause**) | shutdown + ack (the pool-shrink path) | not running → no-op | **no-op** |
| `resume_node` | no-op (wrong mode) | reset + resumed in place, keeps held resources | no-op (wrong mode) | skipped — a manual pause sticks | **no-op** |
| `respawn_terminate` *(async)* | reset + respawned in dep order | untouched (use `resume_pausable`) | left down — the policy regrows it | skipped — a manual stop sticks | **skipped** — it never went down, respawning would double-spawn |
| `resume_pausable` | untouched | reset + resumed in place, keeps held resources | untouched | skipped — a manual pause sticks | **left parked** |

Three flags cut across the modes:

- **`disabled`** is the "a human said stop" latch: `deactivate` sets it (on its seed),
  `activate` clears it, and every bring-up path honors it so a manual stop/pause survives a
  wake respawn or an elastic regrow. Its companion **`collateral`**
  (`TaskNode::is_collateral()`) marks a node stopped only as a *dependent* of a deactivated
  node — the bring-up paths honor it the same way, but `activate` on the ancestor releases
  it.
- **`detached`** (`TaskNode::set_detached(true)`) is full hands-off: the node manages its own
  lifecycle and the supervisor never drives it again. Its `deps:` still order its *first*
  spawn — after that, the graph only remembers where it was declared.

**Defaults in one place:** shutdown-ack timeout **2 s** (override with `ack_timeout:`
for a node whose cleanup legitimately takes longer; a missed ack returns
`FaultKind::ShutdownTimeout`, from the moment that node is signalled on either stop
path); pre-spawn gate budget **100 ms** per node — the
`start()` wave covers all of a node's gates (executor slot, resources, ready
deps) with one budget from when its in-pass deps resolve, while the single-node
verbs bound each gate separately (override with `slot_timeout:`; timeout = a
`NodeFault` naming the gate); control mailbox depth **4** (`request_control`
awaits capacity, `try_request_control` reports `ControlQueueFull`); trace
registries track up to **4** executors and any number of graphs (an intrusive
chain through each graph's `GraphRef`).

## Writing supervised tasks (the TaskNode API)

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
few situations ([which to use](#spawn-vs-task--which-to-use)). Everything below applies
identically to both styles; only who writes the `#[embassy_executor::task]` differs.

**The canonical loop** — race work against shutdown, return on `Err(Aborted)`:

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

The combinators own the select and the ack. Three rules cover the rest:

1. **Race work against shutdown at every blocking await** — `run_cancellable_acked` and
   `run_pausable_loop` own this; use bare `run_cancellable` only when cleanup must run
   between the cancellation and the ack (flush, unpublish, busy/idle bracketing).
2. **Autonomous exit calls `mark_exited()`** — it acks like `ack_dropped()` *and* records
   the completion, so a worker that returns on its own reads as down and a control
   `Activate` can respawn it. `task:` shells do this automatically; `spawn:` tasks call
   it themselves.
3. **Resources follow the mode** — a `Terminate` task re-acquires everything on respawn
   (drop-on-exit is the cleanup); a `Pause` task keeps what it holds across pause→resume
   and never re-acquires.

**`Pause` node** — ack, then park; held resources survive. `run_pausable_loop` owns the
whole protocol:

```rust,ignore
#[embassy_executor::task]
async fn sensor_task(node: &'static TaskNode) {
    let mut bus = acquire_once();                // kept across pause/resume
    node.run_pausable_loop(async || {
        let v = sample(&mut bus).await;          // raced against the pause
        publish(v);
    })
    .await                                       // acks, parks and resumes inside; never returns
}
```

Per-cycle `run_pausable` is the same minus the loop, for a body with its own control
flow between cycles. When cleanup must run between the cancellation and the ack, spell
the tail out: `run_cancellable`, the cleanup, `ack_dropped()`, `wait_resume().await`.

**Pool worker** — same as `Terminate`, plus load reporting around the busy section:

```rust,ignore
node.mark_busy();                                // idle→busy fires the scale signal
serve_connection(&mut socket).await;
node.mark_idle();                                // busy→idle fires it again
```

Keep `mark_busy()` held for the whole session the worker's resource is tied up. The
connection-bound worker that must stay busy *across* a possible cancellation composes
the pieces like this — busy for the whole serve, the return (whose shell ack completes
the handshake) only after the bracketing:

```rust,ignore
loop {
    match node.run_cancellable(socket.accept(PORT)).await {
        Err(Aborted) => return, // idle here: nothing to bracket
        Ok(conn) => {
            node.mark_busy();
            let served = node.run_cancellable(serve(conn)).await; // busy across the race
            node.mark_idle();
            if served.is_err() {
                return; // cancelled mid-serve: bracketed, and the shell acks
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

**Parked node** (declared with no `spawn:`) — the app spawns it by hand, typically
because it needs values only `main` owns; `adopt` keeps trace attribution working:

```rust,ignore
let token = pump_task(&PUMP, hw_handle).unwrap(); // task fns return Result<SpawnToken, _>
PUMP.adopt(&token);                               // register its task id for trace
spawner.spawn(token);                             // Spawner::spawn takes the token
```

### Method reference

The cancellable combinators return `Result<F::Output, Aborted>` and the pausable one
`Result<F::Output, Resumed>`; both markers are crate types —
`use embassy_supervisor::{Aborted, Resumed, TaskNode};` covers the canonical loops.

**Lifecycle methods** — the task-side half of the protocol:

| method | role |
|---|---|
| `run_cancellable_acked(fut).await` | the everyday body: race `fut` against shutdown AND complete the handshake on `Err(Aborted)` — discarding the result (`let _ =`) is fine, the ack already happened |
| `run_cancellable(fut).await` | same race, no ack — for cleanup between the cancellation and the ack |
| `run_pausable_loop(body).await` | the whole `Pause` protocol in one call: rebuilds `body` every cycle, never returns |
| `run_pausable(fut).await` | `Pause` bodies: same race, and on a pause it acks, parks, and returns `Err(Resumed)` only after the resume |
| `wait_shutdown().await` | the underlying primitive: park until a stop/pause is requested |
| `ack_dropped()` | complete the handshake: clears `running`, wakes the supervisor's ack wait — `task:` shells call this on return; `run_cancellable_acked` calls it on `Err(Aborted)`; hand-written `spawn:` tasks call it themselves |
| `mark_exited()` | `ack_dropped()` + record the completion (`has_exited()`) — call on an autonomous exit; `task:` shells emit it automatically |
| `wait_resume().await` | `Pause` only: park (after acking) until resumed |

**Pool and status methods** — load reporting and observability:

| method | role |
|---|---|
| `mark_busy()` / `mark_idle()` | pool workers: report load; a *real* transition fires the scale signal itself |
| `shutdown_requested()` | synchronous check, e.g. at the loop top before starting new work |
| `has_exited()` | true once the last instance's body returned; cleared by the pre-spawn reset |
| `set_detached(true)` | opt out of supervision from now on (self-managed daemon or run-once) |
| `adopt(&token)` | parked nodes: register a hand-spawned task's id so trace accounting sees it |

**Status reads** — readable from anywhere (a status endpoint iterates `GRAPH.nodes`
and reads these; all are cheap atomic loads):

| method | true when |
|---|---|
| `is_running()` | the supervisor has an instance up (spawned, not acked, not exited) |
| `is_busy()` | the instance reported `mark_busy()` |
| `is_disabled()` | stopped-at-boot or control-`Deactivate`d, and not yet re-activated |
| `is_detached()` | self-managed; every lifecycle op skips it |
| `shutdown_requested()` | a stop/pause was requested — readable until the next pre-spawn reset |
| `is_ready()` *(feature `readiness`)* | the task asserted `set_ready()` |
| `is_stale(max_age)` *(feature `liveness`)* | running but no `beat()` within `max_age` |

Useful compositions: **down** = `!is_running()`; **parked `Pause`** = mode `Pause` +
`!is_running()` + `shutdown_requested()`; **autonomous completion** = `has_exited() &&
!shutdown_requested()`. A spawn that fail-closed (a `NodeFault` from a gate) leaves the
node `!is_running()` — nothing was taken or spawned. Ordering guarantees: when
`stop_node`/`teardown`/`deactivate` return `Ok`, the ack has happened and
`is_running()` is already `false`; for bodies that ack by returning (the
`run_cancellable_acked` idiom), `has_exited()` is also already `true` — the ack and
the return land in the same poll.

### Special case: workers that never return

A worker typed `-> !` (or a service contract returning a `Never` type) opts out of
`Terminate`/restart *by type* — the body can never return, so stop and respawn
semantics are inert on it as-is. Two ways in: add
[`cancel`](#cancel--supervisor-unaware-workers) to the node and the generated shell
races the body against shutdown for you (no signature change, the future is dropped
in place), or keep the node argument and race the work yourself (one
`run_cancellable` call) when teardown needs ordered post-cancel work.
`Pause`-parked and detached daemons are the forms that legitimately never return.


## The `supervisor_graph!` DSL

```text
executor NAME;                        // runtime-filled SendSpawner slot (tier / second core)
node NAME = Mode, deps: [A, B][, executor: EXEC], spawn: <spawn>[, disabled];
node NAME = Mode, deps: [A, B][, executor: EXEC], task: <worker>[, pool_size: N]
    [, resources: [[#[cfg(..)]] RES: [local] [shared|consume] Type, ..]]
    [, slot_timeout: MS][, ack_timeout: MS][, cancel][, disabled];
node NAME = Mode, deps: [A];          // neither => parked node the app spawns itself
pool NAME = [Mode, ..], deps: [A][, executor: EXEC],
    spawn: <fn> | task: <worker>,
    [resources: [RES: [local] [shared|consume] Type, ..],]
                                        // take kinds → per-member slot arrays;
                                        // shared (incl. shared local) one pool-wide slot
    policy: [<Type> =] <expr>,
    min: N, max: M[, slot_timeout: MS][, ack_timeout: MS][, cancel];
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
  handle): the pre-spawn gate turns "missing" into a clean `FaultKind::ResourceMissing`.
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
   node MODEM = Terminate, spawn: other_crate::modem_task(&NODES[0]);
   ```

2. **The same task is also spawned outside the graph.** `spawn:` reuses the one existing
   `TaskPool`; `task:` would stamp a second shell + pool — duplicate RAM for the same
   future type.

   ```rust,ignore
   #[embassy_executor::task(pool_size = 2)]
   async fn logger(node: &'static TaskNode, sink: Sink) { /* ... */ }

   // One instance supervised ...
   node LOG = Pause, spawn: logger(uart_sink());
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
   node REPORT = Terminate, executor: HIGH, spawn: report_task(boot_epoch());
   ```

Omitting both keeps the node **parked** (see [Spawn forms](#spawn-forms)) — that's a third
option, not a tie-breaker between the two.

### `resources:` — owned values handed over at spawn

A resource is a value **one party builds and one worker owns while it runs**: a
peripheral split out of `Peripherals`, a driver object, a stream endpoint, a handle an
async bring-up produced. The graph threads it into the worker at spawn instead of
having the body re-acquire it (`Peripherals::steal()`, a global registry looked up by
name), which keeps embassy's compile-time exclusive ownership and turns "not there
yet" into a gate the supervisor waits on rather than a panic inside the task.

`resources: [NAME: Type, ..]` (requires `task:`) emits a `pub static NAME:
ResourceSlot<Type>` at the declaration site. Whoever owns the value fills the slot:

- **`main`**, before `start()`, for anything that exists from boot:

```rust,ignore
async fn blink(node: &'static TaskNode, led: &mut Output<'static>) { /* ... */ }

supervisor_graph! {
    node BLINK = Terminate, task: blink,
        resources: [LED: Output<'static>];
}

// main, after the Peripherals split:
LED.provide(Output::new(p.PIN_25, Level::Low)); // consumes p.PIN_25 — no steal, no 2nd owner
sup.start(&spawner).await?;
```

- **a provider node**, for anything another task builds at runtime (a radio stack
  after its firmware load, a serial stream once its UART is pumping). It names the
  slots it fills with [`provides:`](#provides--slots-that-die-with-their-producer) and the consumers order after it with
  `deps:`; the [provider-node recipe](#provider-node--async-multi-output-construction-in-the-graph)
  shows the whole shape.

The protocol, per (re)spawn:

1. The value is `provide()`d once. Consuming the `Peripherals` field is the
   **compile-time exclusive-ownership guarantee** — a second owner cannot exist.
2. The generated glue probes the slot just before the spawn. An unprovided slot fails
   `Supervisor::start` with `FaultKind::ResourceMissing` after a bounded wait (the
   node's `slot_timeout`, 100 ms by default; the supervisor logs the node name) —
   fail-closed at bring-up, not a panic inside a running task. Provisioning is the
   runtime-checked half of the contract.
3. The generated shell `take()`s the value at its first poll — never through the task-fn
   call, where a `Busy` storage claim would drop it, unrecoverable — hands the worker
   `&mut Type` (after the node arg, in declared order, before any partial-call extras) and
   `restore()`s it after the worker returns (i.e. after its shutdown ack). A Terminate
   respawn therefore re-takes the **same instance**; a Pause worker never returns, so it
   simply retains its resources.

The supervisor awaits a node's slots being filled before each (re)spawn (same bounded wait
as `executor` slots), so late provisioning and the respawn-vs-restore window on another core
are both covered. Caveats: a panic in the worker skips the restore (embedded panic = reboot);
`pool_size > 1` cannot combine with lend/consume/divisible entries — the slot holds ONE
value (a budget, one claimant), so the macro rejects it (use `shared`, or an `ElasticPool`,
whose members get per-member slot arrays).

`ResourceSlot<T>`'s hand-usable API, for providers and app code (the macro's glue uses
the same calls): `provide(T)` / `restore(T)` fill the slot (restore is provide, named for
the give-it-back half), `take() -> Option<T>` empties it, `get() -> Option<T>`
(`T: Copy` only) copies without emptying, `clear()` empties it and resets the filled
latch, and `async wait_take() -> T` awaits a fill then takes — how an `exit:` slot is
read. `provide` on an already-filled slot overwrites (the old value is dropped): every
slot is a mailbox, not a log.

#### Resource kinds: `local`, `consume`, `shared`, and `divisible`

Per-entry markers (order-free; `local` composes with either of the mutually exclusive
`consume`/`shared`; `divisible` stands alone) refine the default lend-and-restore
protocol. The kind follows from what the worker does with the value:

- it only **borrows** it, and the same instance should serve the next activation →
  the default;
- it **moves it into a constructor** (`Uart::new(periph, ..)`), drops it at teardown,
  or the node is one-shot by construction (its setup claims `StaticCell`s a second run
  would re-initialise) → `consume`: the slot stays empty afterwards, so a respawn fails
  at the supervisor with `ResourceMissing` instead of panicking inside the worker;
- several nodes need the **same `Copy` handle** → `shared`;
- several nodes each need a **share of one quantity** (a power budget, a bandwidth
  cap) → `divisible`, feature `budget`.

| kind | worker receives | on worker exit | use for |
|---|---|---|---|
| *(default)* | `&mut Type` | `restore()`d — respawn re-takes the same instance | long-lived singletons (`Output`, a reborrowable `Peri`) |
| `consume` | `Type` **by value** (shell `take()`s) | nothing — the slot stays **empty** | resources the worker must *drop* at teardown (a driver whose `Drop` releases pins/DMA) or that go stale across a power cycle and are rebuilt each run |
| `shared` | `Type` **by value** (shell **copies** via `get()`, `T: Copy`) | nothing — the slot **stays filled** | one handle fanned out to many consumers (`embassy_net::Stack`, a `&'static` shared-bus ref); several nodes — and whole `task:` pools — declare the SAME slot name |
| `local` | as the kind it composes with | as the kind it composes with | `!Send` values (`RefCell`-/`NoopRawMutex`-based driver handles) on a **single core** |
| `divisible` | a `Claimant` **by value**: its own slot in the graph's `Budget<K>` | its share is **released** by the supervisor (`Pause` parks keep it) | one quantity split among N holders, where a dead holder must not strand its share |

`consume` makes teardown-drop explicit and turns the wake path into "build fresh, `provide()`,
respawn": until the application re-provides, a respawn fail-closes with `FaultKind::ResourceMissing`
instead of reusing a stale instance.

`shared` replaces the panicking-accessor pattern for fan-out handles: instead of a
`task:` extra like `stack()` that panics at first poll when the value is missing, a
`shared` resource is gate-awaited before the spawn and a missing value is a clean
`FaultKind::ResourceMissing`. The slot static is emitted once per unique name (with the union of
the declaring sites' `#[cfg]` predicates); every re-declaration must repeat the same
kind markers and type. Entries may also carry per-entry `#[cfg(...)]` — gate the worker
fn's matching parameter with the same attribute.

`shared serialized` adds a compile-time rule to a shared slot: **every holder must run on
one executor**. This prevents a low-priority holder from starving a high-priority waiter
on a serialized bus such as RS-485 or shared SPI. The check is syntactic and costs nothing
at runtime.

`divisible` (feature `budget`) declares a shared budget: `resources: [POWER: divisible]`.
The graph emits one `pub static POWER: Budget<K>` with one slot per declaring node and pool
member. The capacity is `provide()`d at runtime; an unprovided budget is
`FaultKind::ResourceMissing`. Each holder receives a `Claimant` and can state a want, read its
grant, or wait for grant changes. An allocator task calls `POWER.rebalance(&policy, ..)` to
redistribute. Two policies ship: `FairShare` and `ShrinkFastGrowSlow`.

A holder's share is released when it stops. If the shutdown ack never comes, the supervisor
releases the share itself. A `Pause` ack keeps the claim.

```rust,ignore
async fn session(node: &'static TaskNode, power: Claimant) {
    power.want(7_000); // watts, say
    let mut allowed = power.grant();
    let _ = node.run_cancellable_acked(async {
        loop {
            enforce(allowed);
            allowed = power.wait_grant_change(allowed).await; // a cut lands here first
        }
    }).await;
}

supervisor_graph! {
    node SITE   = Terminate, task: site_manager, provides: [POWER];
    pool EVSE   = [Terminate, OnDemand, OnDemand, OnDemand], deps: [SITE], task: session,
        resources: [POWER: divisible], policy: DeferredShrink::new(..), min: 1, max: 4;
}
```

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
    node RADIO = Terminate, task: radio,
        resources: [RUNNER: local consume Cyw43Runner];
}

// bring-up (and again on every wake cycle, BEFORE the respawn):
RUNNER.provide(build_radio_runner().await);
```

#### `provides:` — slots that die with their producer

`provides: [RES, ..]` on a node names the slots its task fills at runtime, resolved
against the graph's `resources:` entries (an unknown name is a compile error). The
node's shutdown ack clears them — the value drops, the filled latch resets; `Pause`
parks excepted, since a parked task still backs what it published — so a consumer's
gate waits for the next activation's value instead of taking a leftover.

**The `shared` slot is why the clause exists.** A `consume` slot is empty from teardown
until the rebuild, so its gate is fresh by construction — but a `shared` slot is never
emptied by its consumers, and "filled" cannot say *whose activation* filled it: after
the provider stops, a gate wait would hand out the previous cycle's handle. Resources
are not couplings, so nothing else links a slot to the task that fills it; the clause
is that link. Emptiness is then the freshness signal the gate waits already
understand. The declared form also covers providers whose ack happens inside
`run_cancellable_acked` or a `cancel` shell, and autonomous exits through
`mark_exited`; the manual form — `ResourceSlot::clear()` before the ack — needs no
feature.

The graph hands the producer nothing: its task names the static itself (`&STREAM` as a
`task:` extra when the body lives in another crate, the bare name when it does not) and
calls `provide()` once the value exists. A runner whose output nobody consumes has no
consumer to declare a slot for it; a hand-written `static X: ResourceSlot<T> =
ResourceSlot::new()` gives it somewhere to provide into, outside the graph's view.

#### Resource or signal?

Both are `pub static`s that two nodes touch, and the graph has a clause for each. They
record different relations:

| | resource (`resources:` / `provides:`) | signal (`reads:` / `writes:` / `#[dataflow]`) |
|---|---|---|
| what it is | a value one party builds and one worker owns | a `'static` that exists from boot and any number of tasks touch |
| lifetime | its provider's; cleared when the provider stops | the program's |
| relation | ownership hand-over, once per activation | runtime coupling, for the whole run, may be cyclic |
| gating | the consumer's spawn waits for the value (`slot_timeout`) | none, unless the signal is wrapped in a gate (`Backed`, `node.open`) |
| examples | a `Peri`, a driver, a `Stack` handle, a stream endpoint | a `Watch`, `Signal`, `Channel`, `Mutex` |

The test is **whether the thing exists before its producer runs**. A `static W:
Watch<..>` does; a UART stream does not. So a signal is never a resource merely because
a consumer must not read it before its writer is up — that is readiness (`deps: [X
ready]`, `ready_on_write`, or a [gated read](#gated-reads-feature-data-deps)), and the
signal stays a coupling. Conversely, a handle a task builds and another task must not
start without is a resource even when it is a channel endpoint or a `&'static Signal`
living inside the provider's state: what crosses the edge is the producer's lifetime,
which is exactly what a `provides:` slot encodes.

Two consequences:

- A `'static` primitive both nodes can already name is a coupling. Threading a reference
  to it through a slot adds a gate that fires at once and a clear-on-shutdown that
  clears nothing; declare it in `reads:`/`writes:` instead.
- When a consumer needs a **value** and a **state** from one provider (a stream, and
  "the link is up"), the value goes through the slot and the state through `ready`.
  Where the value *is* the state — the provider has nothing to say beyond "here it is"
  — the slot gate alone is the rendezvous, and a `ready` marker beside it states the
  same fact twice.

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
fails the spawn with `FaultKind::ExecutorSlotEmpty` — loud, not silent. Constraints: `executor:` requires
a `spawn:` fn (it cannot combine with a verbatim closure), and the routed task's future must
be `Send`.

### Dependencies

`deps:` names declared nodes *or pools*. A pool name resolves to the pool's **floor member**
(member 0, the `min`-kept one), so `deps: [POOL]` means "start after the pool is up".

**A plain dep orders *spawns*, not *readiness*.** `start()` spawns a node and immediately
marks it running, so a dependent with no gates can race its provider's body. Bring-up
is a wave: a node spawns once its deps are up and its gates (executor slot, resource
slots, ready deps) are satisfied, bounded by its own `slot_timeout`; unrelated nodes do
not wait on each other's gates. Two rendezvous exist, both opt-in, and they carry
different things across the edge:

- a `resources:` slot wait — the provider hands over a **value**; the gate is the
  value's presence ([`provides:`](#provides--slots-that-die-with-their-producer));
- a **`ready` dep marker** *(feature `readiness`)* — the provider asserts a **state**
  with nothing to hand over: `deps: [NET ready]` additionally
  awaits the dep's task-asserted `set_ready()` before spawning this node, bounded by
  this node's `slot_timeout` (then `FaultKind::ReadyDepTimeout { dep }`, which names
  the dep that never asserted). Elastic-pool growth also defers while a ready-marked dep is
  un-ready (a sync check per evaluation — no wait). `ready` on a pool name means the
  floor member's readiness; markers on a `pool`'s own `deps:` apply to every member.

The provider side is three calls: `set_ready()` once serving, `clear_ready()` on a
lost link (**status, not control** — dependents are not stopped; pair with a control
`Deactivate` for a cascade, or opt an individual edge into `bound` below), and the
pre-spawn reset clears it so a respawned provider re-asserts. `wait_ready()` exists for
app code too, with the same single-pre-fill-waiter caveat as the other latching gates —
fan N waiters out through an app-owned `embassy_sync::watch::Watch` instead.

Pick by what crosses the edge: a value wants a slot, a state wants `ready`. Both on one
edge for one fact is redundant ([Resource or signal?](#resource-or-signal)).

What a dep edge does *not* say — the continuous flow of data between running
tasks — has its own part: [Dataflow supervision](#dataflow-supervision).

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
- **`shared` with `consume`** — one value cannot both fan out and move by value.
- **`divisible` with any other kind marker, or with a type** — a budget is its own kind.
  Also rejected without the `budget` feature, with more than 256 slots, or with
  `pool_size > 1` (one claimant per slot).
- **`serialized` without `shared`** — serialization applies only to fan-out slots. Also
  rejected when holders span multiple executors.
- **`veto` on a `reads:` entry** — vetoes are asserted, not read. Also rejected without
  the `veto` feature, with more than 32 writers, or when one gate is spelled two ways.
  A `veto` target must be a `VetoGate` with enough slots.
- **a `shared` slot re-declared with different kinds/type** — all declarations of the same
  name must match exactly.
- **`local` resources with `executor:`** — on a node or a pool: a local slot carries
  `!Send` values; a `SpawnerSlot`-routed spawn needs a `Send` future
- **`slot_timeout: 0`** — would fail every gated spawn instantly
- **`ack_timeout: 0`** — would fault every stop before the task's first poll
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
iff any entry is `local`, the local slot type; one `Budget<K>` per `divisible` name), and
`pub static GRAPH` — nothing else.

## Dataflow supervision

Everything above orders *when tasks run*. This part is about the other half of the
graph: *what data flows between them* while they run — declared, turned into a
heartbeat and a readiness assertion, and (at the top tier) derived straight from the
code. It is layered: each feature below builds on the previous one, and stopping at
any layer is fine.

### Spawn ordering is not runtime coupling

A `deps:` edge orders **spawns** — consumed once, at bring-up, never again.
Dataflow — who writes and who reads each signal, for the whole run — is a
different relation, and conflating them cuts both ways: an ordering edge read as
"A feeds B", a real coupling left undeclared because "the deps already say it".
One declaration per meaning:

| declaration | relates | applies |
| --- | --- | --- |
| `deps: [X]` | spawn order | one instant, at bring-up |
| `deps: [X ready]` | spawn order + a startup rendezvous | one instant |
| `resources:` / `provides:` | ownership of a value | once per activation, gated on the value |
| `reads:` / `writes:` / derived tables | dataflow | whole run, **may be cyclic** |
| `deps: [X ready bound]` | runtime state propagation | continuous, opt-in |

- Dataflow may be cyclic; a spawn DAG cannot — which is why the coupling tables
  never feed the topological sort.
- A `deps:` edge is never re-evaluated: nothing re-gates a running node when a
  provider cycles underneath it (`epochs` lets it notice; `restart` re-gates it).
- Readiness is a rendezvous, not a persistent guarantee: without `bound-deps`, a
  provider that comes up and later goes quiet is invisible.
- A resource is not a coupling either: it is a value handed over, bound to its
  provider's lifetime. [Resource or signal?](#resource-or-signal) draws the line.

### The pieces, in plain terms

The record dataflow supervision keeps: **for each signal, which node writes it,
which nodes read it.** What each side gets from being on it:

- **Writer**: found by `GRAPH.writers_of(&SIG)`, drawn by the diagram tool; with
  the features below, the write can become the node's heartbeat and its
  readiness ("ready" = actually producing). Readers' gates resolve against it.
- **Reader**: `readers_of(&SIG)` answers "who is affected if this producer
  cycles?"; the diagram tool warns on one-sided signals. Reads carry no
  bookkeeping — the record is the whole product.

Two ways onto the record, differing in who keeps it true:

- **Declared** — `reads:` / `writes:` lists on the node
  ([below](#declaring-the-dataflow-feature-coupling)). Works for any body;
  nothing checks the list against the code.
- **Derived** — `#[embassy_supervisor::dataflow]` on a fn that accesses signals
  through its node: `node.put(&SIG, v)` / `node.get(&SIG)` perform the access,
  `node.writer(&SIG)` / `node.reader(&SIG)` hand the signal back. The attribute
  scans for those calls and emits the fn's tables beside it — the call site *is*
  the declaration, so it cannot drift.

The graph binds derived tables with two keywords:

- **`discover`** — use the `task:` fn's own table as the node's declaration.
- **`dataflow: [path]`** — adopt annotated helper fns. The scan sees one body at
  a time, so helpers must be annotated and adopted by their callers; a module of
  them adopts as one `#[dataflow_bundle]` entry (`dataflow: [crate::api::BUNDLE]`).

Use cases:

| you are a… | use |
| --- | --- |
| writer or reader in your own task fn | the verbs + `#[dataflow]` + `discover` |
| writer through a setter other nodes call | annotate the setter; each caller adopts it — the write attributes to the caller, and the static stays private to its module |
| reader through an accessor | same: annotate it, callers adopt it |
| writer or reader in a body that cannot see the supervisor (third-party, [`cancel`](#cancel--supervisor-unaware-workers)) | `writes:` / `reads:` lists (`observed beat` adds liveness) |
| writer the scan cannot follow (runtime-computed index) | keep that entry in the list |
| reader of a value meaningless until its writer runs | gate it: `deps: [X ready]` (the spawn waits) or a [gated read](#gated-reads-feature-data-deps) `node.open(&SIG)` (each use waits, resolved via the declared writer) |

All of it composes per node — one table per source (list, task fn, each
adoption). An access on no bound table still works; the graph just doesn't see
it.

### Declaring the dataflow (feature `coupling`)

The hand-written tier, for bodies that cannot carry a node. Entries name the
**actual signal statics**: checked to exist and be `Sync`, so a rename breaks
every referring declaration — and that is the whole check; the list is never
verified against the body.

```rust,ignore
node CONTROLLER = Terminate, deps: [ESKF ready],
    reads:  [crate::signals::ESKF_ESTIMATE],
    writes: [crate::signals::MOTOR_SETPOINT],
    task: crate::controller::entry;
```

The couplings beside a node's deps also say which edges carry data and which
merely sequence: a `deps:` edge with no coupling beside it is pure ordering
("run me last"), and now says so.

The next two sections build on declared entries without touching the body
(`observed` liveness, `ready_on_write`); the sections after them cover the
derived tier in detail.

### Liveness and readiness by polling (feature `coupling-observe`)

**`observed` gives the supervisor a way to ask whether this signal moved; `beat` is
what currently asks.** The marker names an accessor whose result changes when the
signal is written; a consumer decides whether to call it. Nothing is asked of the
task — no node in its signature, no `beat()`, no `set_ready()` — which makes this
the tier for a body you do not own.

Today the heartbeat is the only consumer, so an `observed` entry without `beat` is
inert: declared, one word of flash, and never called. The same is true of every
`observed` entry in a `reads:` list, since `beat` on a read is a compile error. They
are not errors — a second consumer (an input that went quiet, a rate readout) would
give them meaning without a syntax change — but nothing asks about them today.

```rust,ignore
observe writes: it.load(Ordering::Relaxed);   // graph-level default; `it` is the signal

node ESTIMATOR = Terminate, beat_timeout: 1000,
    writes: [crate::signals::ESTIMATE observed],
    task: crate::estimator_task;
```

The accessor resolves from three places, most specific first: `observed via <expr>` on
the entry, the graph-level `observe writes:/reads:` default for its direction, and —
with neither — the `Observable` trait from `embassy-supervisor-observe`. That last one
is a leaf facade in the `log` mold: a signal library implements it (or wraps a primitive
in the facade's `Counted`) without ever depending on the supervisor, which is what a
trait defined *here* could never offer. The atomics implement it out of the box, value
as token. The expression forms remain what reach an accessor no trait method could — a
different accessor per entry, or one element of an array (`ARR[1] observed`).

Two things follow, and they are the point of the feature:

- **With `liveness-monitor`, a `beat`-qualified write drives the node's heartbeat.** The
  sweep treats an advancing counter as a beat, so the node above calls `beat()` only on
  its steady path where it produces no output. Name the wrong signal and the node goes
  **stale** — the declaration stops being a comment.

  `beat` is a separate qualifier because the two questions are different: `observed`
  says the signal can be asked, `beat` says the answer is this node's sign of life. A
  node with four outputs usually wants only one treated that way — without the split,
  marking a second write would quietly weaken the heartbeat from "the output that
  matters moved" to "any output moved". `beat` on a `reads:` entry is a compile
  error — a heartbeat is something a node produces.
- **`ready_on_write`** turns that same advance into the node's readiness assertion,
  described below.

### `ready_on_write`

A node clause: the sweep calls `set_ready()` the first time one of the node's `observed`
writes advances, so "ready" means *actually producing* rather than *reached the line that
says so*. It replaces the common shape of awaiting a first publication and then asserting
readiness by hand.

```rust,ignore
node ESKF = Terminate, beat_timeout: 1000, ready_on_write,
    writes: [crate::signals::ESKF_ESTIMATE observed beat];
```

Requires an `observed beat` entry in `writes:` — the sweep's poll of that write is
what asserts the readiness — and `beat_timeout:`, because the sweep only visits nodes
with a budget. Both are compile errors otherwise, because either alone would be a
silent no-op. A body that carries its own heartbeat through the verbs asserts its own
readiness too, with `set_ready()` at the same write. (A `discover` node reaches the
polled form with a marker-only entry beside it, below.)

**Monotone: it never withdraws readiness.** A node that later goes quiet is reported
through `wait_health()`, and what that means is the application's decision, exactly as
with `liveness-monitor`. Withdrawing readiness here would leave a node permanently unready
with nothing able to restore it — if you want that coupling, the composition already
exists: `wait_health()` → `Stale` → your own `clear_ready()`.

**The boundary, stated plainly.** Polling can only ever watch a signal the
declaration names; it can never see what a body actually touched, and its resolution
is the sweep interval. That is the price of asking nothing of the task. The tier
below crosses both limits at the price polling exists to avoid: the body must see
the supervisor.

### The node as the access path (feature `dataflow`)

`observed` is the implicit tier: the supervisor asks the signal, and neither the
signal crate nor the task body ever learns it exists. The explicit tier is for a body that holds its
`TaskNode` — the split AUTOSAR ships as `Rte_IWrite` beside `Rte_Write`: the access goes
*through* the node, and is thereby the record.

```rust,ignore
#[embassy_supervisor::dataflow]
async fn eskf_task(node: &'static TaskNode) {
    let mut imu = node.reader(&IMU_DATA).receiver().unwrap();  // pass-through: handles
    loop {
        let est = fuse(imu.changed().await);
        node.put(&crate::signals::ESTIMATE, est);              // Sink: the verb writes
        node.beat();                                           // the sign of life
    }
}

// graph: node ESKF = Terminate, deps: [IMU ready], task: eskf_task, discover;
```

`#[dataflow]` scans the fn for calls through its `TaskNode` parameter, emits the fn's
read/write coupling tables as flash `static`s beside it, and rewrites each call site to
carry its table entry — nothing is registered, nothing is declared, and the record
cannot drift from the code. `put`/`get` perform the operation themselves through the
facade's `Sink`/`Source` traits (the atomics implement both; a signal library adds
one-line impls); the pass-through pair exists for the two patterns no value verb can
express — read-modify-write has no value to hand over without racing
(`node.writer(&COUNT).fetch_add(1, ..)`), and a consuming read needs per-consumer
handle state a shared static cannot carry. A derived table states couplings and
nothing else: the sign of life is carried by the verb, `node.beat_put(&SIG, v)` /
`node.beat_writer(&SIG)`, or by a `node.beat()` call. The scan is receiver-keyed on the
node parameter, so a
`map.get(&key)` elsewhere is never touched; an aliased receiver or a helper fn's
accesses are outside it, so annotate helpers too and adopt their tables — an access
no bound table carries still performs, it simply leaves the graph unaware of it.

### Verbs of your own

The verbs are inherent methods on `TaskNode`, so an extension trait can add more; what
the scan needs is to know the ident and which way it points. The attribute's arguments
say so, and say nothing else:

```rust
pub trait Signals {
    fn subscribe<T: Sync + ?Sized>(&'static self, s: Sig<T>) -> &'static T;
    fn publish<T: Sink + Sync>(&'static self, s: Sig<T>, v: T::Item);
}
impl Signals for TaskNode { /* `s.target` is the signal, `s.entry` its table row */ }

#[dataflow(read(subscribe), write(publish))]
async fn entry(node: &'static TaskNode) {
    let rx = node.subscribe(&crate::ESTIMATE);   // lands in reads:
    node.publish(&crate::ARMED, true);           // lands in writes:
}
```

A registered verb takes `Sig<T>`, which is where the rewrite puts the table entry; hand
the signal back with `s.target`, as `reader` does. Registrations are **additive** — the
built-in verbs are always recognised, and naming one of them is an error rather than a
silent redefinition — and **per fn**, so the same method is an ordinary call in a fn
that does not register it, with its coupling simply absent from that fn's tables.

Direction is stated rather than inferred because the scan is token-level and has no
type information, and direction is not cosmetic: `writers_of`/`readers_of`, the
heartbeat and the gate's producer lookup all partition on it. The diagram tool reads
the same attribute, so a registered verb reaches the diagrams with no configuration of
its own, drawn under its own name.

A crate with a house verb set will want its own attribute wrapping `#[dataflow(..)]`,
since the registration otherwise repeats on every annotated fn — that needs a
proc-macro crate on your side, and is the main ergonomic cost of adopting this.

What the record feeds depends on the node's shape:

- **`discover` (the body is the declaration).** The node binds the tables its `task:` fn's
  `#[dataflow]` attribute derived, in place of `reads:`/`writes:` lists — flash-const,
  and true by construction: the entry exists because the call does, so there is no
  second place to keep in sync. The signal queries and the diagram tool see those
  edges.

  A derived table states couplings and marks none, so a list may sit beside
  `discover` **to add markers only**: every entry must carry `observed` and/or
  `beat`, and must name a signal the scan already found. An unmarked entry is a
  spanned error (it would declare a coupling the scan missed), and a marked one
  that no bound table carries fails a const assertion at expansion. That buys a
  heartbeat and `ready_on_write` without a second authority over the relation:

  ```rust,ignore
  node ESKF = Terminate, deps: [IMU ready], task: eskf_task,
      beat_timeout: 1000, discover,
      writes: [crate::signals::ESKF_ESTIMATE observed beat];
  ```

  The check matches the path's **last segment**, because a const context cannot
  compare addresses. A signal reached through a renaming re-export fails it
  although it is legitimate, and two signals sharing a final segment pass it —
  in which case the marker simply never matches a write at runtime and the node
  reads as stale. Otherwise the node beats with `node.beat()` and asserts
  readiness with `node.set_ready()`.
- **Declared lists.** Where derivation cannot follow the code — a runtime index, a
  `#[cfg]`-gated access, a table walked at runtime — keep the lists and route the
  accesses through the node anyway, and let `beat_put`/`beat_writer` carry the
  heartbeat at the write that proves it — no marker, no sweep, no clock read.

**Encapsulated signals keep their privacy.** A signal accessed only through a
getter/setter stays fully private: the accessor takes the caller's node, carries
`#[dataflow]`, and callers adopt it —

```rust,ignore
// heartbeat.rs — the static never leaves the module
static PERIOD_MS: AtomicI32 = AtomicI32::new(500);

#[embassy_supervisor::dataflow]
pub fn set_period_ms(node: &'static TaskNode, ms: i32) {
    node.put(&PERIOD_MS, ms);
}

// the caller's declaration adopts the accessor's table; the write is now the
// caller's coupling, queried and drawn like any other
pool HTTP = [Terminate, OnDemand], deps: [NET ready], task: http_worker,
    dataflow: [crate::heartbeat::set_period_ms], ...;
```

`dataflow: [..]` composes with declared lists and with `discover` alike; a node binds
one table per source (list, task fn, each adopted accessor). A caller that does *not*
adopt the accessor performs the same write, and the graph simply does not attribute it.

A module of accessors adopts as one entry through `#[dataflow_bundle]`:

```rust,ignore
#[embassy_supervisor::dataflow_bundle]     // or #[dataflow_bundle(NAME)]
pub mod api {
    #[embassy_supervisor::dataflow]
    pub fn request_threshold(node: &'static TaskNode, v: u16) { /* node.put(..) */ }
    #[embassy_supervisor::dataflow]
    pub fn signal_config_update(node: &'static TaskNode) { /* node.writer(..) */ }
}

// dataflow: [crate::api::BUNDLE]   — exactly the members' tables, concatenated
```

A member fn's own `#[cfg]` gates every entry it contributes, and the graph cannot
tell a bundle from a fn — the emitted statics share the naming.

The tiers compose per node. Keep `observed` where signals and bodies stay foreign to
the supervisor; go through the node where it is already in hand.

**Teardown is a wave.** Every stop path — `teardown`, `teardown_continue`,
`deactivate`, `restart`'s down half, the `bound-deps` cascade — signals a node the
moment every dependent stopping with it has acked, signals nodes with no such
dependents up front, and re-runs the scan on each ack. That is what lets a producer's
shutdown *wait on* an unordered consumer: a `drain()` reached before the consumer has
acked is released by a consumer that has already been told to go, where signalling one
node at a time would deadlock — the producer holding its ack for the consumer, the
supervisor holding the consumer for the producer. And because a `deps:` dependency is
not even signalled until its dependents are gone, it keeps *serving* through their
cleanup — a dependent may flush over a link, or drive one last ioctl through a runner
it depends on, inside its own shutdown.

The unordered side is the contract to write for. A node with no `deps:` path to
another may be told to stop while that other node is still running, and it frees what
it owns as soon as it acks. So a node that publishes a handle to consumers it has *no
edge from* must hold its own shutdown until they let go — that is what `Leased` is —
and what a node uses while shutting down must be one of its `deps:`; an unordered
service cannot be assumed to still work. A node that only acks and exits is
unaffected, which is most of them.

**Bring-up is a wave too.** `start`, `respawn_terminate`, `activate`
and `restart`'s up half spawn every node whose in-pass deps are up and whose gates test
satisfied on each round, parking between rounds on a gate-event signal fired by
`provide`/`restore`, `SpawnerSlot::set` and `set_ready` — so independent slow bring-ups
overlap instead of queueing, and a provider may even be declared after its consumers
with no dep edge (`tests/bringup_concurrent.rs`). Spawn
ordering is strict: a dependent never spawns before its in-pass deps. The
non-blocking gate test this needs is *emptiness*, which means "valid" for every slot
kind — provided a provider whose values die with it clears its `shared` slots on the
way down ([`provides:`](#provides--slots-that-die-with-their-producer), or `ResourceSlot::clear()` before the ack). A node's
`slot_timeout` covers all its gates together, from when its deps resolve.

**The heartbeat only says something when someone asks**, so high-rate writers stay
cheap. A plain `put`/`writer` costs nothing beyond the write itself; a `beat_put`/
`beat_writer` adds one relaxed store. Reads are pure pass-throughs. The
heartbeat is a flag that whoever next asks about staleness — the monitor's sweep, an
app watchdog, a dashboard — converts into a beat inside `ticks_since_beat`, using the
clock read that call makes anyway. A 1 MHz writer therefore pays a relaxed store per
message, never a timer read, and a beat materializes exactly when someone looks —
which is all `beat_timeout:`/`beat_window:` can resolve anyway. (Readiness is the
exception, asserted at the access: it is once per activation and other nodes'
bring-up waits on it.) One ordering caveat for hot paths: `put` writes
`Relaxed` through the facade, so a flag that publishes other memory keeps its own
ordering via `node.writer(&FLAG).store(v, Release)`.

The diagram tool reads the same source the graph came from with the same scanner the
attribute uses, so `supervisor-mermaid --runtime` draws the derived edges too — by
construction the build's view.

### Gated reads (feature `data-deps`)

Some couplings are not merely observed but *depended on*: an estimate has no meaning
until the node producing it is up. Stating that as `deps: [ESKF ready]` in every
consumer says it once per consumer, by hand, in a place that has no idea which signal
motivated it.

A gate moves the obligation onto the signal. Its **type** carries what reading it
implies, `node.open(&SIG).await` is the verb that discharges it, and consumers say only
that they read:

```rust,ignore
// The declaration says what reading it implies. `Backed` is the gate this crate
// ships: start the producer on first open, then wait for its readiness.
pub static ESTIMATE: Backed<Watch<Estimate>> = Backed::new(Watch::new());

// The consumer states a read. `open` hands back an `Open` guard; `Deref` gives
// the wrapped signal's own API back, and the guard is the reader's hold on the
// producer (below), so bind it for as long as the reading lasts.
let est = node.open(&crate::ESTIMATE).await;
let mut rx = est.receiver();
```

**Nothing names the producer.** The graph already knows who writes a signal, so
`producer_of` looks it up by address over the caller's own graph — which covers
`discover`-derived tables no declaration site could name. It is all const: nothing to
register, nothing to size, and a gate resolves before `Supervisor::start` as readily as
after. Two graphs in one binary never answer for each other.

Write your own by implementing `Gated` for your wrapper: `ensure` receives the reading
node and the coupling entry, so it can log the caller, wait on a mode, throttle a first
access — anything, including awaiting. There is deliberately **no blanket impl**:
`open` on an ungated signal is a compile error, not a no-op the reader would mistake
for a guarantee. Starting the producer needs `control` (the mailbox the request goes
through); without it the gate is the readiness wait alone, which is right for a
boot-started producer and reported for any other.

`open` is the only awaiting verb — a gate fires once per consumer at setup, so the
future belongs there and not on every access — and it grants no exclusive access.
A signal with no gate carries no wrapper, no state and no code.

#### Retiring when the last reader leaves

`Backed` counts its readers. Every `open` is counted before it waits, and the `Open`
guard decrements the count on drop. The producer can retire once the count stays zero
for a cooldown:

```rust,ignore
#[dataflow]
async fn estimator(node: &'static TaskNode) {
    node.writer(&crate::ESTIMATE).send(first_estimate());
    node.set_ready();
    let _ = select(
        node.wait_shutdown(),
        node.retire(&crate::ESTIMATE, Duration::from_secs(5)),
    ).await;
}
```

`retire` waits for `ESTIMATE.openers()` to stay zero for the whole cooldown. A reader
arriving during the cooldown restarts the clock. When it resolves, the producer
withdraws its readiness and, with `control`, requests its own `Deactivate`. A reader
admitted after readiness is withdrawn waits for the next activation instead of reading
a stopped producer. `Backed::unwatched(cooldown)` is the same wait without the automatic
stop. `Open::signal()` returns the wrapped signal for APIs that need a `'static`
reference, but references kept past the guard drop are no longer counted.

### Leases: the teardown side (feature `data-deps`)

A gate orders bring-up. The same coupling has an edge on the way down, and it is
the one that bites: a task that published a handle into a `static` — a network
stack, a DMA buffer, a peripheral view — cannot free it while a consumer is
still holding it.

No declaration answers that. `reads:` records that a node *touches* a signal,
never that it is holding something derived from it across an await, and a
coupling table is best-effort by construction: an unadopted helper, a computed
target or a forgotten list entry is simply absent. Ordering a lifetime
invariant on "not mentioned" is not sound. So the holders are counted instead:

```rust,ignore
pub static NET_STACK: Leased<StackCell> = Leased::new(StackCell::new());

// The consumer holds the guard for exactly as long as it uses the value.
let Some(stack) = node.lease(&crate::NET_STACK) else { return };
serve(*stack).await;

// The producer, on its way down, before it frees the backing.
crate::NET_STACK.drain().await;
```

`drain` closes the signal to new leases, then waits for the live ones to drop.
Closing is what makes the count trustworthy: afterwards a consumer that asks
gets `None` — the honest answer, and one it can act on — rather than a handle
about to dangle, so the producer is not racing an asker. A producer that comes
back up calls `reopen`.

`lease` is sync, unlike `open`: there is nothing to wait for, since either the
value is available or its producer is going away. It records a read like
`reader` does, and the diagram draws it as its own kind of edge.

What this buys over a `deps:` edge is exactness. The count covers **every**
holder — including a consumer whose access no table carries, and a `detached`
node, which teardown skips entirely and which therefore escapes any
ordering-based scheme. The failure mode is a leaked guard: `drain` never
returns and the producer misses its shutdown ack, which surfaces as the
ordinary ack timeout naming the producer rather than as a use-after-free.

Costs one `AtomicU32` and one `Signal` per leased signal, and nothing at all
for signals that do not opt in. `Deref` keeps the wrapped signal's own API
reachable, which also means reaching the static directly is uncounted — the
same advisory property a gate has.

### Gates are advisory; make bypass deliberate

`Backed`, `Leased`, and `VetoGate` guard the access path, not the data. The wrapper is
a plain static, so code that names it directly reads the value uncounted and ungated.
Make bypass deliberate by keeping the static private and exposing only `#[dataflow]`
accessors:

```rust,ignore
mod estimate {
    static ESTIMATE: Backed<Watch<Estimate>> = Backed::new(Watch::new());

    #[dataflow]
    pub async fn publish(node: &'static TaskNode, e: Estimate) {
        node.writer(&ESTIMATE).send(e);
    }

    #[dataflow]
    pub async fn subscribe(node: &'static TaskNode) -> Open<Watch<Estimate>> {
        node.open(&ESTIMATE).await
    }
}
```

The derived tables are public, so the graph can bind the accessors by fn path
(`task: estimate::publish, discover`) without naming the signal. Every reader outside
the module goes through `open`. `supervisor-lint --only public-gate` reports any gate
static that is not private.

### Distributed veto (feature `veto`)

Use `VetoGate` when several protection functions share one trip signal and no single
writer owns release. Any contributor can assert the gate; the gate stays asserted until
every contributor releases its own bit.

```rust,ignore
pub static TRIP: VetoGate<8> = VetoGate::new();

supervisor_graph! {
    node PROT_50_51 = Terminate, task: overcurrent, discover, writes: [crate::TRIP veto];
    node PROT_87    = Terminate, task: differential, discover, writes: [crate::TRIP veto];
    node TRIP_LOGIC = Terminate, task: trip_logic, discover, reads: [crate::TRIP];
}

#[dataflow]
async fn overcurrent(node: &'static TaskNode) {
    let veto = node.veto(&crate::TRIP).expect("declared `veto`");
    veto.assert();   // gate asserted while ANY contributor holds it
    // ...
    veto.release();  // clears only this writer's bit
}

#[dataflow]
async fn trip_logic(node: &'static TaskNode) {
    let gate = node.reader(&crate::TRIP);
    loop {
        gate.wait_asserted().await;
        open_breaker();
        gate.wait_released().await; // once every contributor let go
        rearm();
    }
}
```

The macro assigns each `veto` writer a contributor slot in declaration order and checks
at compile time that the target is a `VetoGate` with enough slots. It also rejects mixed
spellings of the same gate, so `TRIP` and `crate::TRIP` cannot both appear. A writer can
move only its own bit, so no writer can clear another's trip. A stopped writer's bit stays
asserted, keeping the trip latched. Under `coupling-observe`, `writes: [crate::TRIP veto
observed beat]` counts trips as the writer's heartbeat. All writers of one gate must live
in the same graph.

### Channels, mutexes, and anything else `Sync`

None of this is about signals. `CouplingPoint` is a blanket impl over every `Sync`
type and identity is the static's address, so a coupling names **any `'static` two
nodes both touch** — a `Channel`, a `Mutex`, a driver handle, whatever the application
shares. embassy-sync's primitives behave nothing like a shared cell (a channel is a
bounded queue with backpressure, a mutex hands out a guard, both acquire
asynchronously) and the layer carries them unchanged:

```rust,ignore
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as Cs;

pub static CHAN: Channel<Cs, u32, 4> = Channel::new();
pub static MUX:  Mutex<Cs, u32> = Mutex::new(0);
pub static GATED: Backed<Channel<Cs, u32, 4>> = Backed::new(Channel::new());

#[dataflow]
async fn producer(node: &'static TaskNode) {
    node.beat_writer(&crate::CHAN).send(1).await;   // heartbeat on a send
    *node.writer(&crate::MUX).lock().await = 7;     // and on a lock
}

#[dataflow]
async fn consumer(node: &'static TaskNode) {
    let _ = node.reader(&crate::CHAN).receive().await;
    let _ = node.open(&crate::GATED).await.receive().await;  // gated receive
}
```

Derived and declared tables, the pass-through verbs, `open` on a
`Backed<Channel<..>>` (whose `Deref` hands the channel's own API back), the
signal-indexed queries and the gate's `producer_of` lookup all work as they do on a
signal. `tests/sync_primitives.rs` pins the lot.

Three things to know:

- **The value verbs do not fit, by design.** `put`/`get` go through `Sink`/`Source`,
  which are **sync and infallible** — last-value-wins over a cell. A bounded `send` is
  async and a `try_send` can fail, so neither primitive implements them and
  `node.put(&CHAN, v)` is a compile error rather than a silent block-or-drop. Use the
  pass-through verbs, or register your own: `fn offer(&self, s: Sig<Channel<..>>, v: u32)
  -> bool` over `try_send` is exactly the shape the built-in verbs have no room for.
  Do not implement `Sink` for a queue to make `put` compile.
- **A queue's `len()` is not a change token.** The `observed` sweep compares successive
  readings **for inequality only**, so a channel that fills and drains between two
  sweeps reads identical and looks silent — which under `liveness-monitor` reports the
  node stale. Wrap it in `Counted` (whose token is the write *count*, and so never goes
  backwards) before pairing `observed` with `beat`. `observed via it.len()` is fine as
  a description; it is wrong as a heartbeat.
- **Direction means less on shared mutable state.** `reads:`/`writes:` records a
  producer→consumer relation; every holder of a mutex does both. Declaring it on both
  sides is honest as "these nodes are coupled through this lock" and misleading as
  "this one produces it". Gating a mutex is a category error outright: `Backed` asks
  "has the node that fills this started producing yet", which is apt for a channel and
  meaningless for a lock that exists from `static` initialisation.

The layer records who touches what, not in what order. Two nodes taking two mutexes in
opposite orders is a coupling the graph will draw and never object to — the `deps:` DAG
is checked for cycles, the coupling table deliberately is not. And a `'static` both
nodes can name is always a coupling, never a resource ([Resource or signal?](#resource-or-signal)).

### A status line (feature `node-status`)

`sd_notify`'s third verb, `STATUS=`. A node describes what it is doing in one line —
`node.report_status("receiving image")` — and `node.status()` hands it to whoever asks:
a dashboard, a shell command, a log line on change. Purely descriptive: never an event,
never acted on, cleared when the node (re)starts so a fresh activation does not wear the
previous one's last words.

### The app-owned health monitor

The supervisor reports; the application decides. Every input is a cheap atomic load:

```rust,ignore
for (i, node) in GRAPH.iter_nodes() {
    let down    = !node.is_running();
    let wedged  = node.is_stale(Duration::from_secs(1)); // `liveness`
    let unready = !node.is_ready();                      // `readiness`
    let gen     = node.epoch();                          // `epochs`
    // GRAPH.deps_of(i) / GRAPH.dependents_of(i, &mut |d| ..) for the topology
}
```

With `liveness-monitor` the polling is done for you: declare `beat_timeout:` on the
nodes whose bodies beat, and consume `wait_health()`.

**Write freshness is not value validity.** The monitor only answers whether a node
wrote recently. `Stamped<T>` lets a reader check a value's age, but that means
"written within `max_age`", not "this exact read is correct". It cannot detect
semantic problems like a clock servo drifting while still writing. The application
must decide what a stale report means.

**What to do about a report is deliberately yours.** Where a subsystem can be cycled
safely, feeding a `Stale` event to `Supervisor::restart` (feature `restart`) or to
`clear_ready()` across a `bound` edge is reasonable. Where it cannot be —
a flight control loop, a motor commutation task, anything holding physical state —
restarting is the wrong reflex and degrading to a safe mode is the right one. The
supervisor cannot tell those apart, so it does not try, and `liveness-monitor` stays
report-only by design.

### Testing a coupling claim

Whether a coupling actually carries data is an app-side test pattern, because only the
app can build a valid sample: with the mock clock, bring the graph up, inject into a
producer's signal, and assert the consumer's `beat()` advances (`ticks_since_beat`) or
its own output changes, within a bounded mock interval.

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
  The spawn glue fallibly boxes the init value (**alloc failure = `FaultKind::Spawn`**,
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
- **`state: zeroed Type`** — the same lifecycle with no init value: the glue calls
  `alloc_zeroed` and hands the worker the block as a live `&mut Type`. With
  `= init_expr` the value is built in the spawner's frame and copied into the Box
  unless the optimizer elides it, so a large buffer set briefly costs its size in
  stack; the zeroed form never does, at any size or opt-level. `Type` must implement
  [`Zeroable`](https://docs.rs/bytemuck/latest/bytemuck/trait.Zeroable.html)
  (re-exported as `embassy_supervisor::Zeroable`): `unsafe impl Zeroable for Bufs {}`
  for a struct of byte arrays, or bytemuck's derive.
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
    node WIFI   = Terminate, task: wifi_ctrl,
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
            sub.start(&spawner).await?;               // WIFI -> NET -> UPLOAD, in order
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
    sup.activate(&UPLOAD, &spawner).await;            // WIFI -> NET -> UPLOAD
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
    node SENSOR   = Terminate, task: sensor_worker;
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
    node BUS = Terminate, task: bus_worker;
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
    node.wait_shutdown().await; // returning acks via the shell's mark_exited
}

supervisor_graph! {
    // `provides:`: these slots die with this node — its
    // shutdown ack clears them, so a respawned consumer's gate waits for the
    // rebuilt value instead of taking the previous cycle's leftover.
    node RADIO_HW = Terminate, task: radio_hw,
        provides: [RUNNER, CONTROL, STACK];
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
values. A provider that dies before providing surfaces as `FaultKind::ResourceMissing` on its
consumers after their `slot_timeout` — fail-closed, never a stale reuse.

What the clause clears, and why the `shared` slot needs it, is under
[`provides:`](#provides--slots-that-die-with-their-producer). The consumers declare
no `ready` dep on the provider: the value is the rendezvous.

### Readiness rendezvous (`ready` dep marker)

A plain dep orders spawns; the `ready` marker (feature `readiness`) additionally
holds the dependent until the dep's task says it is actually serving:

```rust,ignore
supervisor_graph! {
    node NET  = Terminate, task: net_worker;
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
`slot_timeout:` and fails the spawn with `FaultKind::ReadyDepTimeout`, so a provider that never
becomes ready is a loud, retryable error — never a hang. A `bound` edge is the
exception: there the spent budget PARKS the dependent (`BOUND_STOPPED`) instead of
faulting, and the bind loop lifts it when the provider asserts (defer until serving).

`ready` carries a state, not a value. When what the dependent waits for *is* something
the provider hands over (a stream, a stack handle), a `resources:` slot filled under
`provides:` is the rendezvous and `ready` adds nothing; see the provider-node recipe
above and [Resource or signal?](#resource-or-signal).

### Elastic worker pool with `DeferredShrink`

```rust,ignore
supervisor_graph! {
    node BROKER = Terminate, task: broker_worker;
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
    node SENSOR = Pause;   // neither `task:` nor `spawn:` => parked node
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
    node NET     = Terminate, task: net_worker;
    node UPDATER = Terminate, deps: [NET], task: updater_worker, disabled;
}
```

`start()` skips `UPDATER` at boot; it comes up only when runtime control targets it with
`request_control(&UPDATER, ControlOp::Activate)`. Use for on-demand subsystems (a firmware
updater, a debug server) that shouldn't run until explicitly asked for.

### Detached self-managed daemon

```rust,ignore
supervisor_graph! {
    node LOG_DRAIN = Terminate, task: log_drain_worker;
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
    node SAMPLER = Terminate, executor: HIGH, task: sampler_worker;
    node LOGGER  = Terminate, deps: [SAMPLER], task: logger_worker;
}

// app side, before `sup.start(...)` (embassy-rp shown; any HAL works):
static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();
interrupt::SWI_IRQ_0.set_priority(Priority::P2);
HIGH.set(EXECUTOR_HIGH.start(interrupt::SWI_IRQ_0));
```

`SAMPLER` runs at raised priority while `LOGGER` stays on the thread executor — yet the
dependency between them is still honored. `sampler_worker`'s future must be `Send`; if the
slot is never filled, `start()` fails with `FaultKind::ExecutorSlotEmpty` after a bounded wait. A
`task:` extra is evaluated inside the shell, i.e. on the raised-priority tier at its first
poll — switch that node to `spawn:` when an argument must instead be snapshotted on the
supervisor's executor at the moment of the spawn (case 4 of
[`spawn:` vs `task:`](#spawn-vs-task--which-to-use)).

### Second-core pool

```rust,ignore
supervisor_graph! {
    executor CORE1;
    pool CRUNCHERS = [OnDemand, OnDemand], executor: CORE1,
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
    pool WORKERS = [Terminate, OnDemand], 
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
    node NET = Terminate, task: net_worker;
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

    node SENSOR   = Terminate, executor: HIGH, task: sensor_worker;
    node NET      = Terminate, task: net_worker;
    node UPLOADER = Terminate, deps: [NET, SENSOR], task: uploader_worker;
    node STATS    = Pause, task: stats_worker;   // parked through sleep
    node POWER    = Terminate;  // parked: main spawns it with the Spawner
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
        SUP.respawn_terminate(&spawner).await.ok();  // respawn the stateless services
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
`run_pools(&spawner)` future — `select`ed against `wait_control()` in the driver loop — wakes
on each scale request (it never polls), asks each pool's `ScalingPolicy` for a `PoolAction`,
and starts/stops one member accordingly. A member is never grown while one of its declared
dependencies is down (or, with `readiness`, while a `ready`-marked dep is un-ready).

**The whole driver is one call** when you don't need extra select arms:
`sup.run(&spawner).await` = `start()` + drive pools and control forever, returning a
`NodeFault` only on error (bring-up spawn failure, or a missed shutdown ack) — every arm
an app-level escalation, typically `panic!` into a hardware-watchdog reset. Apps that
select their own wake sources into the loop keep writing
`select(sup.run_pools(&spawner), wait_control())` + `apply_control` by hand.

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
    node BENCH = Terminate, executor: CORE1, task: bench_worker, disabled;
}

// core 1 publishes its spawner as it boots (embassy-rp shown; any HAL works):
spawn_core1(p.CORE1, &mut CORE1_STACK, || {
    EXECUTOR1.run(|sp| CORE1.set(sp.make_send()))
});

// bring-up rendezvouses with that asynchronous publish as part of `start` itself
// (bounded wait per `executor:` node, then `FaultKind::ExecutorSlotEmpty`):
sup.start(&spawner).await?;
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
    node NET = Terminate, task: crate::net::net_task,
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

- **Paths**: a fragment references its own workers/types with plain `crate::…`, as
  anywhere else in its crate — the macro normalizes it to `$crate`, which is what
  resolves to the fragment's crate at any compose site (`$crate::…` may also be
  written directly). Another crate's items take a fully-qualified `::crate_name::…`.
  No `$` other than `$crate` is permitted (validated).
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
  still resolve in the trace recorders — each `start()` links its graph into the chain
  the recorders walk, and there is no cap on how many).
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
hook definitions at the graph declaration site (exactly one set may exist per binary —
define your own hooks and forward to the `trace::on_*` recorders if you need custom ones); `metadata-names` stamps node names into task Metadata for external tooling
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
closure-spawned nodes register with one call: `TaskNode::adopt(&token)` — or
`node.adopt_current().await` from inside the task body, when nobody holds the token. The
supervisor's own host task is part of the unsupervised share unless `trace-self` is on:
each graph then carries a hidden `"supervisor"` node that `start()` adopts as its calling
task, attribution being task-granular (everything else that task polls is billed to it).

The hook API is an executor implementation detail, and this feature tracks the executor
minor version the crate pins. It has already changed shape past 0.10: embassy's git main
(the next release) replaces the seven `_embassy_trace_*` symbols with a `raw::trace::Trace`
impl registered through `trace_impl!`, and passes `ExecutorId`/`TaskId` newtypes instead
of `u32`s. Building against that executor takes `RUSTFLAGS="--cfg
embassy_supervisor_trace_v2"` (with `[patch.crates-io]` pointing `embassy-executor` **and**
`embassy-executor-timer-queue` at git — the two must move together, or embassy-time's queue
utils size `TimerQueueItem` through a second copy and timeouts stop firing). The recorders
keep their `u32` keys either way; custom hooks narrow ids with `trace::task_key` /
`trace::executor_key`. CI's canary job builds and tests exactly that configuration, and the
cfg goes away once the crate pins the release that ships it.

## Cargo features

| feature   | default | what it adds |
|-----------|:-------:|--------------|
| `macros`  |    ✓    | the `supervisor_graph!` graph-declaration macro |
| `control` |         | runtime control plane (`ControlOp`, `request_control`, `apply_control`) |
| `pool`    |         | elastic worker pools (`ElasticPool`, `run_pools`, `GRAPH.pools`) |
| `local-resources` | | permit the `local` resource kind — ⚠ opt-in to the macro emitting a documented `unsafe impl Sync` (single-core contract) |
| `budget` | | the `divisible` resource kind: a graph-sized `Budget<K>` of units, a `Claimant` per holder, `FairShare`/`ShrinkFastGrowSlow` policies, and a holder's share released by the supervisor when it stops — including a wedged holder that misses its ack |
| `readiness` | | task-asserted readiness: `set_ready`/`wait_ready`/`clear_ready` + the `ready` dep marker (bring-up + pool-growth gating) |
| `liveness` | | per-node heartbeat: `beat()` raises a flag that `ticks_since_beat() -> u32` converts using the clock read it already makes, plus `is_stale(max_age)` — alive-but-wedged detection without `trace`. A fresh spawn counts as a beat, so a node is never instantly stale. The `beat_put`/`beat_writer` verbs exist only with this on |
| `liveness-monitor` | | the sweep over those heartbeats: `beat_timeout:` / `beat_window:` clauses, `Supervisor::monitor`, `HealthEvent` on `wait_health()`. **Report-only** — escalation is the application's call (implies `liveness`) |
| `epochs` | | per-node activation generation (`epoch()`, `wait_epoch_change()`), so an *already-running* dependent can notice that a provider was restarted underneath it. Pure status |
| `coupling` | | declared dataflow: `reads:` / `writes:` naming real signal statics, and the signal-indexed queries; `Stamped<T>`, the read-side write-freshness wrapper (`age`, `read_fresh`) |
| `coupling-observe` | `coupling` | the `observed` entry marker and its accessor (`via`, graph default, or the `Observable` facade): a way to ask whether a signal moved. `beat` is what currently asks — with `liveness-monitor` it drives the heartbeat and `ready_on_write` by polling, nothing asked of the task |
| `dataflow` | `coupling` | the node as the access path: `#[dataflow]` derives a fn's coupling tables from its `put`/`get`/`writer`/`reader` calls, `discover` binds the task fn's and `dataflow: [..]` adopts accessors' (flash-const, no second place to update); `beat_put`/`beat_writer` carry the node's heartbeat at the write that proves it, and `#[dataflow(read(..), write(..))]` registers verbs of your own |
| `graph-ref` | | a graph as one addressable `'static`: `supervisor_graph!` emits a `GraphRef` beside the node table and `GRAPH.graph_ref` names it. Carries no behaviour of its own — it is the handle `data-deps` and `trace` both need, in opposite directions |
| `veto` | `dataflow` | the `veto` entry marker: one contributor slot of a `VetoGate<N>` per writer, numbered and capacity-checked by the macro; `node.veto(&GATE)` moves only that writer's bit, the actuator parks on `wait_asserted`/`wait_released`; a stopped writer's trip persists |
| `data-deps` | `graph-ref` + `dataflow` | data-driven dependencies, both directions. Bring-up: `node.open(&SIG).await` runs the signal's own `Gated::ensure` first, `Backed<T>` (with `readiness`) starts the producer on first open and holds the reader until it is ready, and `producer_of` finds that producer through the graph by address. Teardown: `Leased<T>` + `node.lease(&SIG)` count the live holders so a producer's `drain()` waits for zero before it frees what it published. Nothing names anything; a signal that uses neither costs nothing |
| `node-status` | | `report_status()`/`status()` — a one-line self-description per node, `sd_notify(STATUS=..)` style; shown when asked, cleared on activation, never an event |
| `restart` | | `Supervisor::restart` — rest_for_one: cycle a node and its transitive dependents, re-gating them on the way back up (implies `control`) |
| `bound-deps` | | the `bound` dep marker — ⚠ **the one feature that changes a documented contract**, per edge and only where you opt in: `clear_ready()` stops a `bound` dependent instead of merely deferring its next spawn, and a bring-up readiness budget spent on a `bound` edge parks the dependent (`BOUND_STOPPED`, lifted by the next `set_ready`) instead of faulting (implies `readiness` + `control`) |
| `heap-state` | | `state: Type = expr` / `state: zeroed Type` per-activation boxed state, reclaimed on task exit — ⚠ opt-in: emits the ~6-line fallible-boxing `unsafe` helper into your crate; needs a `#[global_allocator]`; pulls `bytemuck` for `Zeroable` |
| `defmt`   |         | route the supervisor's logs through `defmt` — on embedded targets (`target_os = "none"`) only, where a `#[global_logger]` exists to link against; takes precedence over `log` there. On a hosted target the feature is inert and `log` is the live backend, so one feature list serves a SITL's both halves and `--all-features` host tests link with no defmt sink |
| `log`     |         | route them through the `log` facade — the live backend on any target with an OS. `init_host_logging(LevelFilter)` (hosted targets only) installs a dependency-free stderr sink in one call, `[uptime] LEVEL target: message`; or install `env_logger` and filter with `RUST_LOG=embassy_supervisor=trace`. With **neither** backend the log macros are no-ops, so the `liveness-monitor` stale reports and every bring-up line print nothing |
| `trace`   |         | trace-hook observability: per-node CPU time / poll counts / max-poll watermark, executor idle time, stall detection |
| `trace-hooks` |     | batteries-included: the graph declaration also defines the executor's trace hooks — the `_embassy_trace_*` symbols, or the `Trace` impl under `--cfg embassy_supervisor_trace_v2` (implies `trace`) |
| `metadata-names` |  | stamp node names into task Metadata for external tooling (rtos-trace/SystemView); independent of `trace` — no hook symbols |
| `trace-names` |     | shorthand for `trace` + `metadata-names` |
| `trace-nested` |    | preemption-exact accounting: nested higher-tier polls are credited back to the window they interrupt (implies `trace`) |
| `trace-self` |      | the supervisor's own host task as a hidden auto-adopted node: `start()` stamps its calling task into a per-graph `"supervisor"` node (`GRAPH.graph_ref.self_node()`), so waves/driver/monitor poll time is attributed instead of landing in the unsupervised share. No declaration needed; the node is outside the node table, so lifecycle never touches it (implies `trace`) |

**Only `macros` is on by default.** Everything the supervisor can *do* is opt-in,
including `control` and `pool`: both add code to the driver loop that runs every
iteration whether or not a graph uses it, so a graph with no pool and no control ops
should not carry either. `restart` and `bound-deps` enable `control` on their own, so
you rarely name it directly.

If you declare a `pool` without the feature, or call a control verb without it, the
macro and the compiler say so by name — the failure mode is a spanned error, not a
silent behaviour change.

## Testing on the host

The crate is HAL-free, so graphs run on a desktop for tests: embassy-executor's
`platform-std` + `executor-thread` features give a std `Executor` to run on a thread,
and `embassy-time`'s `mock-driver` provides the clock (also enable
`critical-section/std`). The whole harness is ~15 lines:

```rust,ignore
#[embassy_executor::task]
async fn driver(spawner: embassy_executor::Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
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
        // Advance ONLY to observe a timeout (a shutdown-ack or gate
        // NodeFault) or liveness staleness — cross-thread advance is sound.
        // clock.advance(embassy_time::Duration::from_millis(500));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
```

A frozen mock clock is fine on the happy paths — every wait resolves by signal (acks,
slot fills, readiness), and the internal timeouts exist only to convert a *failure*
into an error, so advance the clock only when a test wants to observe
a `NodeFault` (a missed ack, an empty gate), or `is_stale` flipping (the liveness clock IS
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
  ack-based check cannot see. One Signal + slice, one AtomicU32 and a beat-flag
  byte per node (the ready flag is a bit in the packed flags word). See [Readiness rendezvous](#readiness-rendezvous-ready-dep-marker).
- **`heap-state`** *(off by default)*. `state: Type = init_expr` or `state: zeroed
  Type` on `task:` nodes and pool members: fallibly boxed per activation (alloc
  failure = `SpawnError::Busy`, retryable), lent to the worker as `&mut Type`, dropped
  on exit before restores — every activation allocates fresh, net zero across
  respawns, while task STORAGE stays static by soundness. See [Heap and the graph](#heap-and-the-graph).
- **Pools grew up.** Take-kind `resources:` entries become per-member slot arrays
  (member `I` owns element `I` exclusively; a lend value survives shrink and regrow
  on the same index), `min:`/`max:` accept const-evaluable expressions guarded by
  const asserts, and `ElasticPool::member_index(node)` indexes per-member app state.
- Also: [`exit: Type`](#exit--typed-exit-values) — the worker's return value lands in
  a generated `<NODE>_EXIT` slot just before the completion is recorded;
  `run_cancellable` / `run_cancellable_acked` as combinators; `resume_node()`, and
  `activate`/`deactivate` now public; and `Supervisor::run(&spawner)`, which is
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
  inside tasks), each (re)spawn probes-then-`take()`s it (unprovided → `SpawnError::Busy`
  out of `start()`, fail-closed), the worker receives `&mut Type`, and the shell
  `restore()`s it on exit so a respawn re-takes the *same instance*. See
  [`resources:`](#resources--owned-values-handed-over-at-spawn).
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

### 0.7 → 0.8

Ships with `embassy-supervisor-macros` 0.9.0 on `embassy-supervisor-syntax` 0.3.0
(both pinned by exact version — no action needed). One signature changes, and the
compiler finds every site:

| 0.7.x | 0.8.0 |
|---|---|
| `node.open(&SIG).await` returned `&'static T` | it returns the gate's `Handle` — an `Open<T>` guard for `Backed`, which `Deref`s to the signal and leaves the reader count on drop. `let est = node.open(&SIG).await; est.receiver()` where `node.open(&SIG).await.receiver()` no longer lives long enough; `.signal()` where a `&'static T` is what you need (a `Leased` inside a `Backed`) |
| a hand-written `Gated` impl had `ensure` only | add `type Handle = &'static Self;` and `fn admit(&'static self) -> &'static Self { self }` (or a counting guard of your own) |
| a `#[dataflow]` body calling a node method named `retire` or `veto` | those are scanner verbs now (the producer's retirement, the writer's veto handle) and their first argument is rewritten to a `Sig` |

Everything else is additive: the `budget` and `veto` features, the `serialized` marker,
`Stamped`, the `public-gate` lint. A `Backed` gains 4 bytes and a `Signal`; a `Coupling`
gains a byte only under `veto`.

### 0.6 → 0.7

Ships with `embassy-supervisor-macros` 0.8.0, unchanged — no action needed. No API
is removed and no signature changes; two control-plane behaviors are corrected.
`deactivate` no longer latches `disabled` on the target's transitive dependents:
they take the new `collateral` hold (`TaskNode::is_collateral()`), which every
automatic bring-up path honors the same way, and which `activate` on the ancestor
releases — restarting the released `Terminate`/`Pause` dependents in the same wave.
Code that read `is_disabled()` on a dependent to detect a subtree stop should read
`is_collateral()`; showing both flags distinguishes "deactivated in its own right"
from "held as a dependent". And an elastic pool now regrows without an app-side
`request_scale()` once its provider recovers: a readiness assertion, a provider
`restart`, `activate`, and the bound recovery path poke the pool driver
themselves, so a manual poke that papered over the parked driver is redundant
(and harmless).

### 0.4 → 0.5

Ships with `embassy-supervisor-macros` 0.7.0 (pinned by exact version — no action
needed). The largest breaking surface so far; the compiler finds every item:

| 0.4.x | 0.5.0 |
|---|---|
| every spawning verb (`start`, `run`, `run_pools`, `start_node`, `respawn_terminate`, `activate`, `apply_control`) took `Spawner` by value | they take `&Spawner` — `sup.run(&spawner)`. The `NodeCfg` spawn fn type still takes `Spawner` by value |
| lifecycle verbs returned `Result<(), ShutdownTimeout>`, `run` a `RunError`, bring-up a raw `SpawnError` | one `NodeFault { node, kind: FaultKind }` everywhere, with an unconditional `Display`; match `fault.kind` against `ExecutorSlotEmpty`, `ResourceMissing`, `ReadyDepTimeout { dep }`, `Spawn(SpawnError)`, `ShutdownTimeout` |
| `Graph<N>` / `Supervisor<N>`, edges read off `GRAPH.deps` / `GRAPH.order` | a `Topology` parameter — annotate with the macro's alias, `Supervisor<5, GRAPH_TOPOLOGY>` — and edges read through `GRAPH.deps_of(i)` / `GRAPH.order()` |
| `TaskNode` carried `name`/`mode`/`spawn` fields and the `with_*` builders | flash `NodeCfg` + RAM handle: read `name()` / `mode()`; the builders live on `NodeCfg`; a hand-built node is two statics (macro graphs are unaffected) |
| default features `["control", "pool", "macros"]` | just `["macros"]` — name `control`/`pool` explicitly where used (`restart`/`bound-deps` still pull `control` in; a `pool` without the feature is a spanned macro error) |
| `trace::register_graph(&NODES)`, capped at 4 graphs | `trace::register_graph(GRAPH.graph_ref)`, no cap — only hand-registered graphs are affected, `Supervisor::start` registers for you |
| exhaustive `match` on `ControlOp` | the enum is `#[non_exhaustive]` (and gains `Restart` under the `restart` feature) |
| sequential bring-up and teardown | concurrent waves, order guarantees intact. The contract to re-check: an **unordered** node may be told to stop while others still run; a node publishing a handle to consumers it has no edge from holds its own shutdown with `Leased` + `drain`; whatever a node uses *during* its shutdown must be one of its `deps:` |
| gate waits budgeted 100 ms per gate | one `slot_timeout` budget per node in the `start()` wave; the single-node verbs still bound each gate |

Behavioural changes needing no code edit: `clear_ready()` stays status-not-control
unless an edge opts in with `bound`; the monitor wakes per earliest-due node instead
of on a global `min(beat_timeout)/2` period; resources no longer cross the spawn
call, so a failed claim cannot drop a `lend`/`consume` value.

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
| `sup.run_pools(&spawner).await` never returned | returns `ShutdownTimeout` (only on a shrink whose member missed its ack) |
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
| `sup.start(spawner)?` | `sup.start(&spawner).await?` |
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
