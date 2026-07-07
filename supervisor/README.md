# embassy-supervisor

[![crates.io](https://img.shields.io/crates/v/embassy-supervisor.svg)](https://crates.io/crates/embassy-supervisor)
[![docs.rs](https://docs.rs/embassy-supervisor/badge.svg)](https://docs.rs/embassy-supervisor)

A generic, **HAL-agnostic** task-lifecycle supervisor for the [embassy](https://embassy.dev)
async embedded framework. `no_std`, no allocator, no board crates — it compiles for any embassy
target. The only third-party deps are pure-embassy crates (`embassy-executor`/`-sync`/`-time`/
`-futures`) and `portable-atomic`.

## Table of contents

- [What it is](#what-it-is)
- [Highlights in 0.3.1](#highlights-in-031)
- [Quickstart](#quickstart)
- [The model](#the-model)
- [Lifecycle reference](#lifecycle-reference)
- [Writing supervised tasks (the TaskNode API)](#writing-supervised-tasks-the-tasknode-api)
- [The `supervisor_graph!` DSL](#the-supervisor_graph-dsl)
- [Recipes by use case](#recipes-by-use-case)
- [Elastic pools](#elastic-pools)
- [Multi-executor tiers and multi-core](#multi-executor-tiers-and-multi-core)
- [Observability](#observability)
- [Cargo features](#cargo-features)
- [no_std / MSRV](#no_std--msrv)
- [Full example](#full-example)
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
  endpoint, a button, …) through a decoupled mailbox (`request_control` / `apply_control`) that
  honors dependencies and pool membership.
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
tasks do — it orchestrates their *lifecycle* and leaves the rest to you.

## Highlights in 0.3.1

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

## Quickstart

```rust,ignore
use embassy_executor::Spawner;
use embassy_supervisor::{supervisor_graph, Supervisor, wait_control};

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
    sup.start(spawner).await.expect("initial spawn"); // brings up `net`, then `app`
    loop {
        let cmd = wait_control().await;               // runtime control requests
        sup.apply_control(cmd, spawner).await;        // applied in dependency order
    }
}
```

`start` is `async` because an `executor:` node first awaits its slot; a plain single-executor
graph resolves immediately — the `.await` costs nothing.

## The model

Three pieces, all `static`:

- **`TaskNode`** — one per managed task: a name, a `Mode`, an optional spawn fn, and a
  private handle of atomic flags + signals. The *task side* of the protocol is a handful of
  node methods — see [Writing supervised tasks](#writing-supervised-tasks-the-tasknode-api).
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

How a task implements its half of these transitions is the
[TaskNode API](#writing-supervised-tasks-the-tasknode-api).

## Lifecycle reference

The canonical per-operation matrix — what each supervisor operation does to a node, by mode
and by the two lifecycle-spanning flags (`disabled`, `detached`). Other docs link here.

| operation | `Terminate` | `Pause` | `OnDemand` | disabled | detached |
|---|---|---|---|---|---|
| `start` *(boot, async)* | spawned in dep order | spawned; a parked (no-`spawn:`) node is only marked running | skipped | skipped | spawned like any node — tasks detach *themselves* after their first spawn |
| `teardown` | shutdown + ack, exits | shutdown + ack, parks on `wait_resume()` | stopped if running, else skipped | already down — nothing to do | **skipped** (self-managed) |
| `deactivate` *(control)* | disabled + stopped; cascades to transitive dependents, dependents first | disabled + stopped, parks; stays parked | disabled + stopped — the whole pool, atomically | re-disabled (idempotent) | **skipped** — never pulled into the cascade, even when targeted directly |
| `activate` *(control)* | enabled + started, after its transitive deps | enabled + resumed in place | enabled only — the pool policy regrows it under load | this is the flag it clears | **skipped** — not re-enabled, not restarted; its `deps:` are start-ordering only and are not expanded |
| `stop_node` | shutdown + ack | shutdown + ack, parks | shutdown + ack (the pool-shrink path) | not running → no-op | **no-op** |
| `respawn_terminate` *(async)* | reset + respawned in dep order | untouched (use `resume_pausable`) | left down — the policy regrows it | skipped — a manual stop sticks | **skipped** — it never went down, respawning would double-spawn |
| `resume_pausable` | untouched | reset + resumed in place, keeps held resources | untouched | skipped — a manual pause sticks | **left parked** |

Two flags cut across the modes:

- **`disabled`** is the "a human said stop" latch: `deactivate` sets it, `activate` clears it,
  and every bring-up path honors it so a manual stop/pause survives a wake respawn or an
  elastic regrow.
- **`detached`** (`TaskNode::set_detached(true)`) is full hands-off: the node manages its own
  lifecycle and the supervisor never drives it again. Its `deps:` still order its *first*
  spawn — after that, the graph only remembers where it was declared.

## Writing supervised tasks (the TaskNode API)

A supervised task is an async fn whose first parameter is its node — the macro's glue
passes it automatically; extra arguments come from the partial-call form
(`task: my_task(EXTRA)`). The preferred style is a **plain worker fn** declared with
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
   or on each pause (`Pause`) *before* parking. A task that never acks panics the
   supervisor after a timeout with the node's name — a loud bug report, not a hang.
3. **An autonomous exit also acks** — a worker backing off on its own calls
   `ack_dropped()` too, so the pool sees it as down and can re-grow it later.
4. **Resources follow the mode**: a `Terminate` task re-acquires everything on respawn
   (drop-on-exit is the cleanup); a `Pause` task keeps what it holds across
   pause→resume and never re-acquires.

Task-side methods:

| method | role |
|---|---|
| `wait_shutdown().await` | park until a stop/pause is requested (returns immediately if already requested) |
| `shutdown_requested()` | synchronous check, e.g. at the loop top before starting new work |
| `ack_dropped()` | complete the handshake: clears `running`, wakes the supervisor's ack wait |
| `wait_resume().await` | `Pause` only: park (after acking) until resumed |
| `mark_busy()` / `mark_idle()` | pool workers: report load; a *real* transition fires the scale signal itself — no manual `request_scale()` needed |
| `set_detached(true)` | opt out of supervision from now on (self-managed daemon or run-once — see the [lifecycle reference](#lifecycle-reference)) |
| `adopt(&token)` | parked nodes: register a hand-spawned task's id so trace accounting sees it |

**`Terminate` / `OnDemand` worker** — the canonical select loop:

```rust,ignore
#[embassy_executor::task]
async fn worker_task(node: &'static TaskNode) {
    let mut conn = acquire();                    // re-acquired on every respawn
    loop {
        if node.shutdown_requested() {
            node.ack_dropped();
            return;                              // drop(conn) is the cleanup
        }
        match select(conn.serve(), node.wait_shutdown()).await {
            Either::First(res) => handle(res),
            Either::Second(()) => {
                node.ack_dropped();
                return;
            }
        }
    }
}
```

**`Pause` node** — ack, then park; held resources survive:

```rust,ignore
#[embassy_executor::task]
async fn sensor_task(node: &'static TaskNode) {
    let mut bus = acquire_once();                // kept across pause/resume
    loop {
        'active: loop {
            match select(sample(&mut bus), node.wait_shutdown()).await {
                Either::First(v) => publish(v),
                Either::Second(()) => break 'active,
            }
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
keep-alive connection): the policy only shrinks non-busy workers.

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
let token = pump_task(&PUMP, hw_handle);         // build the SpawnToken first
PUMP.adopt(&token);                              // register its task id for trace
spawner.spawn(token).unwrap();
```

## The `supervisor_graph!` DSL

```text
executor NAME;                        // runtime-filled SendSpawner slot (tier / second core)
node NAME = Mode, deps: [A, B][, executor: EXEC], spawn: <spawn>[, disabled];
node NAME = Mode, deps: [A, B][, executor: EXEC], task: <worker>[, pool_size: N]
    [, resources: [RES: Type, ..]][, disabled];
node NAME = Mode, deps: [A];          // neither => parked node the app spawns itself
pool NAME = [Mode, ..], deps: [A][, executor: EXEC],
    spawn: <fn> | task: <worker>,
    policy: [<Type> =] <expr>,
    min: N, max: M;
```

### Spawn forms

A bare path `f` spawns `f(&NAME)`; a partial call `f(a, b)` spawns `f(&NAME, a, b)` (the node
is always injected first); a closure is emitted verbatim (nodes only). These forms apply to
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
  accessors (a spawn batch polls last-first).
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

### `#[cfg(...)]`

Allowed on any `node`/`pool` *and on individual deps*. Absent nodes keep their slot as `None`
and are skipped everywhere at runtime.

### `pool`

The mode list declares the members (floor first: typically `[Terminate, OnDemand, ...]`). The
macro generates the member array `NAME: [TaskNode; K]`, per-member spawn glue, a
`NAME_POOL: ElasticPool<P>`, and the structural constants `NAME_MIN` / `NAME_MAX` /
`NAME_MEMBERS` (`usize`). Pool fields are positional and fixed:
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
  across the whole graph
- **`resources:` on a `pool`** — members would contend for a single instance; declare
  per-node
- **pool bounds** — `min <= max <= K` (member count), values must fit `u8`
- **pool without the `pool` feature** — a `pool` item requires enabling it
- **more than 256 slots** — the `u8` index cap above
- **dependency cycle** — caught by the `const` topological sort, so it surfaces at
  const-eval of `GRAPH` rather than at macro expansion; still a compile error

Generated surface at the call site: one `pub static` per node, the pool array + `NAME_POOL`
\+ the `NAME_MIN`/`NAME_MAX`/`NAME_MEMBERS` consts,
one `SpawnerSlot` static per `executor NAME;`, and `pub static GRAPH` — nothing else.

## Recipes by use case

Node and pool names below are invented; swap in your own task fns.

### Simple dependency chain

```rust,ignore
supervisor_graph! {
    node SENSOR   = Terminate, deps: [], spawn: sensor_task;
    node REPORTER = Terminate, deps: [SENSOR], spawn: reporter_task;
}
```

`REPORTER` is brought up only after `SENSOR`. The topological order is computed at compile
time — a cycle or an unknown dep name is a compile error.

### Generic worker over N driver types (`task:`)

```rust,ignore
// ONE generic worker — a plain async fn, not a #[embassy_executor::task]:
async fn poll_sensor<D: Sensor>(node: &'static TaskNode, dev: D) {
    loop {
        match select(dev.sample(), node.wait_shutdown()).await {
            Either::First(v) => publish(v),
            Either::Second(()) => return node.ack_dropped(),
        }
    }
}

supervisor_graph! {
    node BUS = Terminate, deps: [], spawn: bus_task;
    // One node per concrete driver; the macro stamps a monomorphized shell each:
    node BME = Terminate, deps: [BUS], task: poll_sensor::<Bme280>(bme());
    node SHT = Terminate, deps: [BUS], task: poll_sensor(sht());  // inferred
}
```

Args (`bme()`, `sht()`) are evaluated inside each shell at first poll, on the
node's own executor.

### Elastic worker pool with `DeferredShrink`

```rust,ignore
supervisor_graph! {
    node BROKER = Terminate, deps: [], spawn: broker_task;
    pool WORKERS = [Terminate, OnDemand, OnDemand, OnDemand], deps: [BROKER],
        spawn: worker_task,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: 1, max: 4;
}
```

Four member slots; `min: 1` is the always-on floor, growth up to `max: 4` under load.
`DeferredShrink` waits 4 s of idle surplus before shrinking so brief lulls don't thrash.
Requires the `pool` feature.

### Pause node holding a resource (parked, app-spawned)

```rust,ignore
supervisor_graph! {
    node SENSOR = Pause, deps: [];   // no `spawn:` => parked node
    node READER = Terminate, deps: [SENSOR], spawn: reader_task;
}

// main() spawns the sensor task itself, with the peripheral handle it owns:
spawner.spawn(sensor_task(&SENSOR, i2c)).unwrap();
```

A `Pause` node acks a shutdown, then parks on `wait_resume()` — the I2C handle it holds is
never dropped. `resume_pausable()` thaws it in place after a wake.

### Control-started node (`disabled`)

```rust,ignore
supervisor_graph! {
    node NET     = Terminate, deps: [], spawn: net_task;
    node UPDATER = Terminate, deps: [NET], spawn: updater_task, disabled;
}
```

`start()` skips `UPDATER` at boot; it comes up only when runtime control targets it with
`request_control(&UPDATER, ControlOp::Activate)`. Use for on-demand subsystems (a firmware
updater, a debug server) that shouldn't run until explicitly asked for.

### Detached self-managed daemon

```rust,ignore
supervisor_graph! {
    node LOG_DRAIN = Terminate, deps: [], spawn: log_drain_task;
}

#[embassy_executor::task]
async fn log_drain_task(node: &'static embassy_supervisor::TaskNode) {
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
    node SAMPLER = Terminate, deps: [], executor: HIGH, spawn: sampler_task;
    node LOGGER  = Terminate, deps: [SAMPLER], spawn: logger_task;
}

// app side, before `sup.start(...)` (embassy-rp shown; any HAL works):
static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();
interrupt::SWI_IRQ_0.set_priority(Priority::P2);
HIGH.set(EXECUTOR_HIGH.start(interrupt::SWI_IRQ_0));
```

`SAMPLER` runs at raised priority while `LOGGER` stays on the thread executor — yet the
dependency between them is still honored. `sampler_task`'s future must be `Send`; if the slot
is never filled, `start()` fails with `SpawnError::Busy` after a bounded wait.

### Second-core pool

```rust,ignore
supervisor_graph! {
    executor CORE1;
    pool CRUNCHERS = [OnDemand, OnDemand], deps: [], executor: CORE1,
        spawn: cruncher_task,
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
        spawn: worker_task,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
    node DISPATCHER = Terminate, deps: [WORKERS], spawn: dispatcher_task;
}
```

A dep on a pool name resolves to the pool's **floor member**, so `deps: [WORKERS]` means
"start `DISPATCHER` once the pool floor is up".

### Run-once check, ordered last

```rust,ignore
supervisor_graph! {
    node NET = Terminate, deps: [], spawn: net_task;
    pool WORKERS = [Terminate, OnDemand], deps: [NET],
        spawn: worker_task,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
    node READY_PROBE = Terminate, deps: [WORKERS], spawn: ready_probe_task;
}

#[embassy_executor::task]
async fn ready_probe_task(node: &'static embassy_supervisor::TaskNode) {
    node.set_detached(true);
    // everything above is up now; do a one-shot post-boot self-check, then return
}
```

`deps: [WORKERS]` on a leaf node makes it the last thing brought up; detaching lets it exit
without ever being waited on by a teardown.

### Composite: sensor tier + parked diagnostics + power coordinator

```rust,ignore
supervisor_graph! {
    executor HIGH;                    // interrupt-priority tier

    node SENSOR   = Terminate, deps: [], executor: HIGH, spawn: sensor_task;
    node NET      = Terminate, deps: [], spawn: net_task;
    node UPLOADER = Terminate, deps: [NET, SENSOR], spawn: uploader_task;
    node STATS    = Pause, deps: [], spawn: stats_task;   // parked through sleep
    node POWER    = Terminate, deps: [];  // parked: main spawns it with the Spawner
}

static SUP: Supervisor<5> = Supervisor::new(&GRAPH);

// A parked node (no `spawn:`): main spawns it by hand because it needs a value
// only main has — here the `Spawner` that `respawn_terminate` takes:
//     spawner.spawn(power_task(&POWER, spawner)).unwrap();
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
dependencies is down.

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
    node BENCH = Terminate, deps: [], executor: CORE1, spawn: bench_task, disabled;
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
custom ones); `trace-names` stamps node names into task Metadata for external tooling
(SystemView, debuggers); `trace-nested` makes accounting preemption-exact — a nested
higher-tier poll credits its time back to the window it interrupted (register
`trace::set_core_id_fn` on multi-core for one preemption stack per core).

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
| `defmt`   |         | route the supervisor's logs through `defmt` (otherwise the log macros are no-ops) |
| `trace`   |         | trace-hook observability: per-node CPU time / poll counts / max-poll watermark, executor idle time, stall detection |
| `trace-hooks` |     | batteries-included: the graph declaration also defines the `_embassy_trace_*` hook symbols (implies `trace`) |
| `trace-names` |     | stamp node names into task Metadata for external tooling (implies `trace`) |
| `trace-nested` |    | preemption-exact accounting: nested higher-tier polls are credited back to the window they interrupt (implies `trace`) |

`default-features = false` gives a minimal core that only does dependency-ordered
bring-up/teardown — dropping the control plane and pools trims flash and a couple of statics.

## no_std / MSRV

`#![no_std]` and `#![forbid(unsafe_code)]`. Requires Rust 1.85+ (edition 2024). The embassy
dependencies are pre-1.0 (`embassy-executor` 0.10, `embassy-sync` 0.8, `embassy-time` 0.5), so a
consuming application must use compatible embassy minor versions.

## Full example

The [`firmware`](https://github.com/cedrivard/embassy-supervisor/tree/main/firmware) crate in the
repository is a complete working application on an RP2350 — networking, an HTTP control plane, an
elastic worker pool, multi-executor tiers on both cores, trace observability, and OTA firmware
update — all driven by this supervisor.

## Migration

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
