# embassy-supervisor

[![crates.io](https://img.shields.io/crates/v/embassy-supervisor.svg)](https://crates.io/crates/embassy-supervisor)
[![docs.rs](https://docs.rs/embassy-supervisor/badge.svg)](https://docs.rs/embassy-supervisor)
[![docs](https://img.shields.io/badge/docs-embassy--supervisor.github.io-blue)](https://embassy-supervisor.github.io/)


**Run-time supervision for embassy firmware**: a dependency-ordered task-lifecycle
supervisor and a declared-and-verified dataflow layer, for the
[embassy](https://embassy.dev) async embedded framework. The graph declares *when
each task runs* (bring-up order, modes, pools, executor placement) and *what data
flows between them* (reads/writes declared, polled, or derived straight from the
code), the first checked at compile time. **HAL-agnostic**,
`no_std`, no allocator, no board crates: it compiles for any embassy target. The only
third-party deps are pure-embassy crates (`embassy-executor`/`-sync`/`-time`/
`-futures`), `portable-atomic` and `pin-project-lite`.

It brings tasks up in dependency order, supervises their lifecycle (`Terminate`,
`Pause`, `OnDemand`), tears dependents down before the things they depend on, and can
police declared heartbeats and data freshness.

## What it is

- **Dependency-ordered lifecycle**: the supervisor brings tasks up in dependency order and
  tears dependents down before the things they depend on.
- **Lifecycle modes**: `Terminate` (started at boot, restartable), `Pause` (park/resume while
  keeping a held resource), `OnDemand` (started on demand to scale a pool).
- **Elastic pools** *(feature `pool`)*: `ElasticPool` scales a set of single-instance worker
  nodes with load via a swappable `ScalingPolicy` (e.g. `DeferredShrink`), within a fixed budget.
- **Runtime control** *(feature `control`)*: drive start/stop/pause/resume from anywhere (an HTTP
  endpoint, a button, …) through a decoupled, **lossless** mailbox (`request_control` awaits
  capacity; the sync `try_request_control` reports a full queue instead of dropping) and a
  dependency- and pool-aware `apply_control`.
- **Whole-graph lifecycle ops**: `teardown()` / `teardown_continue()` (reverse-dependency
  shutdown with acked handshakes, running as a dep-ordered wave: a dependency
  keeps serving until its stopping dependents have acked, unordered nodes stop
  concurrently), `respawn_terminate()`
  (dependency-ordered re-spawn after a wake), `resume_pausable()` (thaw parked nodes); a
  missed shutdown ack comes back as a `NodeFault` naming the node and what went wrong,
  never a hang or a library panic.
- **Multi-executor placement**: `executor:` annotations route nodes onto interrupt-priority
  tiers; the graph is the single source of *where each task runs*.
- **Multi-core placement.** The same mechanism spans the second core: `start()` rendezvouses
  with the other core's asynchronous executor bring-up as part of the bring-up loop, and a whole
  elastic pool can live on core 1, scaled by core 0's supervisor.
- **Safe resource threading**: `resources:` annotations hand owned values to workers
  through `ResourceSlot`s: peripherals moved in from `main` (compile-time exclusive
  ownership, no `steal()`), or values another node builds at runtime and `provides:`.
  The consumer's spawn waits for the value; a lent value is restored on exit so a
  respawn re-takes the same instance.
- **Dataflow declaration** *(feature family `coupling`)*: `reads:`/`writes:`
  entries name the actual signal statics tasks exchange data through, so the graph can
  answer who produces a signal and who consumes it, and the diagram tool can draw it.
- **Dataflow-driven liveness** *(features `coupling-observe`, `dataflow`)*: a
  declared write can be the node's *sign of life*: polled from outside (`observed beat`,
  with no change to the task) or carried by the access (the `beat_put`/`beat_writer` node verbs).
  `#[dataflow]` goes further and **derives** a task's read/write tables from its body at
  compile time, so the declaration cannot drift from the code.
- **Health surface** *(features `liveness`, `readiness`, `node-status`)*: per-node
  heartbeat + staleness, task-asserted readiness that gates dependents, and a one-line
  self-description; all cheap atomic reads for an app-owned monitor.
- **Observability** *(feature family `trace`)*: per-node CPU time, poll counts and stall
  detection by consuming embassy-executor's trace hooks, with node *names* attached.

The supervisor deliberately does **not** allocate, own a HAL, manage power states, or know what your
tasks do: it orchestrates their *lifecycle* and checks their *dataflow*, and leaves the rest to
you. It also does **not**
catch panics: a panicking task is not captured or restarted (panic capture is off the table in a
`forbid(unsafe_code)` no_std library; it would need unwinding or the app-global panic handler).
Pair the supervisor with a hardware watchdog for crashes, and the `liveness` heartbeat for
alive-but-stalled tasks.

## The grammar, at a glance

The whole DSL is three item kinds (`node`, `pool`, `executor`), each one line, comma-
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
    default executor NAME;              // ..and the tier every task:/spawn:-fn item without executor: inherits

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
        , writes: [crate::SIG observed beat] //     `observed beat` marks the heartbeat write (polled)
        , discover                      //   or: bind the tables the task fn's #[dataflow] derived
                                        //     (a list beside it may only add markers)
        , dataflow: [crate::setter]     //   adopt an accessor fn's #[dataflow] tables
        , beat_timeout: MS , ready_on_write //   every value-level clause (the timeouts, beats,
                                        //     ready_on_write, disabled, discover, enabled_if:,
                                        //     restart_budget:) takes a #[cfg(...)] gate
        , beat_window: N                //   1..=255 consecutive stale sweeps before a report
        , enabled_if: crate::settings::lora_on //   fn() -> bool; false at start() latches disabled
        , restart_budget: MAX in MS     //   restart() refuses the MAX+1th inside the window
        , executor: NAME , slot_timeout: MS , ack_timeout: MS , disabled;

    pool NAME = [Mode, ..],             // one Mode per member, floor first
        deps: [..], task: worker,
        resources: [..],                //   take kinds become per-member slot arrays
        policy: DeferredShrink::new(..),// scaling policy (required)
        min: EXPR, max: EXPR            //   required; any order, like every clause
        , cancel;                       //   same flag, applied to every member
}
```

Reading rules, all regular: node and pool clauses may appear **in any order**
(only the mode after `=` is positional; for a pool, the mode list); a repeated
clause is a compile error; list entries are separated by commas (a trailing one is
fine); the timeouts and `restart_budget:` take plain integer literals (no `ms`, `s` or
type suffix); every clause is **inline on
its item** (there are no block forms and no top-level `resources { }` section); a
pool always names its `policy:`; take-kind resource names are **globally unique**
(each is one static; only `shared` names repeat, by design); and `detached` is not a
mode but a runtime call (`TaskNode::set_detached`). Anything structurally wrong (a
dependency cycle, an unknown name, a duplicate) is a compile error with a message,
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

// Plain async fns taking the node first; no embassy attribute needed.
async fn net_task(node: &'static embassy_supervisor::TaskNode) { /* ... */ }
async fn app_task(node: &'static embassy_supervisor::TaskNode) { /* ... */ }

#[embassy_executor::task]
async fn supervisor_task(spawner: Spawner) {
    // Infallible: the order is precomputed, so a dependency cycle is a compile error.
    let sup = Supervisor::new(&GRAPH);
    // Bring-up in dependency order. A failure is one `NodeFault` whose `Display`
    // names the node and the cause; the app escalates it (typically a panic into
    // a hardware-watchdog reset).
    if let Err(fault) = sup.start(&spawner).await {
        panic!("supervisor: {fault}");
    }
}
```

With the `control` or `pool` feature, `sup.run(&spawner).await` replaces the `start`
call: it runs a best-effort `start`, then the driver loop, and returns a `NodeFault` only
on a missed shutdown ack or, with `restart-intensity`, a refused restart.
The driver loop serves control commands (`wait_control` + `apply_control`), pool scaling
(`pool_pass` once per wake, then `select(wait_scale(), Timer::at(next))` on the instant it
returns, `wait_scale()` alone on `None`) and, with `bound-deps`, bound-dependency changes
(`wait_bind` + `apply_bind`); with `liveness-monitor` it polls `monitor()` beside them. A
driver you write yourself must serve every one of these that its features enable, from one
loop that finishes each pools pass, command or bind cascade before taking the next, and
after each bind cascade applies at most one queued command before waiting again (a
provider that flaps while each cascade waits would otherwise starve control). A
`select(run_pools, wait_control)` loop alone never stops or restarts `bound` dependents
and never reports a stale node. Bring-up is `async` because an `executor:` node first
awaits its slot; a plain single-executor graph resolves immediately, so the `.await`
costs nothing.

## Documentation

The reference guide ships with the API docs, one chapter per topic:

- [The two tiers](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/tiers/index.html): code that holds its node, and code that never sees the supervisor
- [The model](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/model/index.html): nodes, graph and supervisor, plus the lifecycle reference
- [Writing supervised tasks](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/tasks/index.html): the `TaskNode` API
- [The `supervisor_graph!` DSL](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/dsl/index.html): every clause, resources and compile-time checks
- [Dataflow supervision](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/dataflow/index.html): declared, polled and derived couplings
- [Recipes by use case](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/recipes/index.html)
- [Elastic pools](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/pools/index.html)
- [Multi-executor tiers and multi-core](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/executors/index.html)
- [Composing graphs across crates](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/composing/index.html)
- [Observability](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/observability/index.html): tracing, lifecycle events, the bring-up breadcrumb and fault injection
- [Testing on the host](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/testing/index.html)

The [website](https://embassy-supervisor.github.io/) has a getting-started guide and the
in-browser [playground](https://embassy-supervisor.github.io/playground/).

## Cargo features

| feature   | default | what it adds |
|-----------|:-------:|--------------|
| `macros`  |    ✓    | the `supervisor_graph!` graph-declaration macro |
| `control` |         | runtime control plane (`ControlOp`, `request_control`, `apply_control`) |
| `pool`    |         | elastic worker pools (`ElasticPool`, `run_pools`, `pool_pass`, `GRAPH.pools`) |
| `local-resources` | | permit `local` resources: opt-in to the macro emitting `unsafe impl Sync`; the one-executor contract is checked per slot |
| `budget` | | the `divisible` resource kind: a graph-sized `Budget<K>` of units, a `Claimant` per holder, `FairShare`/`ShrinkFastGrowSlow` policies, and a holder's share released by the supervisor when it stops, including a wedged holder that misses its ack |
| `readiness` | | task-asserted readiness: `set_ready`/`wait_ready`/`clear_ready` + the `ready` dep marker (bring-up + pool-growth gating) |
| `liveness` | | per-node heartbeat: `beat()` raises a flag that `ticks_since_beat() -> u32` converts using the clock read it already makes, plus `is_stale(max_age)`: alive-but-stalled detection without `trace`. A fresh spawn counts as a beat, so a node is never instantly stale. The age saturates at `u32::MAX / 2` ticks (about 35.8 min at a 1 MHz tick) while something checks it at least that often, so a stalled node stays stale; a `max_age` past that is clamped just below it, and a `beat_timeout:` at or past it is refused at compile time. The `beat_put`/`beat_writer` verbs exist only with this on |
| `liveness-monitor` | | the sweep over those heartbeats: `beat_timeout:` / `beat_window:` clauses, `Supervisor::monitor`, `HealthEvent` on `wait_health()`; plus `GRAPH.all_fresh()` / `GRAPH.stale_nodes()`, the synchronous hardware-watchdog feed gate. **Report-only**: escalation is the application's call (implies `liveness`) |
| `epochs` | | per-node activation generation (`epoch()`, `wait_epoch_change()`), so an *already-running* dependent can notice that a provider was restarted underneath it. Pure status. `wait_epoch_change()` serves one waiter at a time; other dependents compare `epoch()` |
| `coupling` | | declared dataflow: `reads:` / `writes:` naming real signal statics, and the signal-indexed queries; `Stamped<T>`, the read-side write-freshness wrapper (`age`, `read_fresh`) |
| `coupling-observe` | | the `observed` entry marker and its accessor (`via`, graph default, or the `Observable` facade): a way to ask whether a signal moved. `beat` is what currently asks: with `liveness-monitor` it drives the heartbeat and `ready_on_write` by polling, nothing asked of the task (implies `coupling`) |
| `dataflow` | | the node as the access path: `#[dataflow]` derives a fn's coupling tables from its `put`/`get`/`writer`/`reader` calls, `discover` binds the task fn's and `dataflow: [..]` adopts accessors' (flash-const, no second place to update); `beat_put`/`beat_writer` carry the node's heartbeat at the write that proves it, and `#[dataflow(read(..), write(..))]` registers verbs of your own (implies `coupling`) |
| `graph-ref` | | a graph as one addressable `'static`: `supervisor_graph!` emits a `GraphRef` beside the node table and `GRAPH.graph_ref` names it. Carries no behaviour of its own; it is the handle `data-deps` and `trace` both need, in opposite directions |
| `veto` | | the `veto` entry marker: one contributor slot of a `VetoGate<N>` per writer, numbered and capacity-checked by the macro; `node.veto(&GATE)` moves only that writer's bit, the actuator parks on `wait_asserted`/`wait_released`; a stopped writer's trip persists (implies `dataflow`) |
| `data-deps` | | data-driven dependencies, both directions. Bring-up: `node.open(&SIG).await` runs the signal's own `Gated::ensure` first, `Backed<T>` (with `readiness`) starts the producer on first open and holds the reader until it is ready, and `producer_of` finds that producer through the graph by address. Teardown: `Leased<T>` + `node.lease(&SIG)` count the live holders so a producer's `drain()` waits for zero before it frees what it published. Nothing names anything; a signal that uses neither costs nothing (implies `graph-ref` + `dataflow`) |
| `node-status` | | `report_status()`/`status()`: a one-line self-description per node, `sd_notify(STATUS=..)` style; shown when asked, cleared on activation, never an event |
| `restart` | | `Supervisor::restart` (rest_for_one): cycle a node and its transitive dependents, re-gating them on the way back up (implies `control`) |
| `restart-intensity` | | the `restart_budget: MAX in MS` clause: `restart` refuses the MAX+1th restart of a node inside the window with `FaultKind::RestartStorm`, before stopping anything, and `apply_control` propagates it (implies `restart`) |
| `enabled-if` | | the `enabled_if: fn() -> bool` clause: `start()` evaluates it for each down node and latches `disabled` on `false`: settings-gated bring-up |
| `lifecycle-events` | | `wait_lifecycle()` / `try_wait_lifecycle()`: a lossy 8-slot queue of `LifecycleEvent { node, kind }` for every driven transition, with a drop counter |
| `lifecycle-sink` | | `set_lifecycle_sink(fn(LifecycleEvent))`: the same events, pushed synchronously to an app callback on the supervisor task; no queue, no drops |
| `breadcrumb` | | `node.crumb()` and the packed `crumb_word()` / `decode_crumb()`: the last lifecycle phase, per node and binary-wide, for postmortem stashing |
| `bound-deps` | | the `bound` dep marker: opt-in contract change where `clear_ready()` stops the dependent and a spent readiness budget parks it instead of faulting (implies `readiness` + `control`) |
| `heap-state` | | `state: Type = expr` / `state: zeroed Type` per-activation boxed state, freed when each activation ends. ⚠ opt-in: emits a small fallible-boxing `unsafe` helper per `state:` form into your crate (the `zeroed` one relies on your `Zeroable` impl); needs a `#[global_allocator]`; pulls `bytemuck` for `Zeroable` |
| `fault-inject` | | `TaskNode::inject(Fault::{Stall, Wedge, Crash, Hog(..)})` / `clear_fault()` / `fault()`: faults done TO a task (the shell withholds polls, drops the future or busy-spins; the node hides a stop and swallows the ack), see [Fault injection](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/observability/index.html#fault-injection). A bench, test and demo feature |
| `defmt`   |         | route the supervisor's logs through `defmt`, on embedded targets (`target_os = "none"`) only, where a `#[global_logger]` exists to link against; takes precedence over `log` there. On a hosted target the feature is inert and `log` is the live backend, so one feature list serves a SITL's both halves and `--all-features` host tests link with no defmt sink |
| `log`     |         | route them through the `log` facade: the live backend on any target with an OS. `init_host_logging(LevelFilter)` (hosted targets only) installs a dependency-free stderr sink in one call, `[uptime] LEVEL target: message`; or install `env_logger` and filter with `RUST_LOG=embassy_supervisor=trace`. With **neither** backend the log macros are no-ops, so the `liveness-monitor` stale reports and every bring-up line print nothing |
| `trace`   |         | trace-hook observability: per-node CPU time / poll counts / max-poll watermark, executor idle time, stall detection (implies `graph-ref`) |
| `trace-hooks` |     | batteries-included: the graph declaration also defines the executor's trace hooks: the `_embassy_trace_*` symbols, or the `Trace` impl under `--cfg embassy_supervisor_trace_v2` (implies `trace`) |
| `metadata-names` |  | stamp node names into task Metadata for external tooling (rtos-trace/SystemView); independent of `trace`, so no hook symbols |
| `trace-names` |     | shorthand for `trace` + `metadata-names` |
| `trace-nested` |    | preemption-exact accounting: nested higher-tier polls are credited back to the window they interrupt (implies `trace`) |
| `trace-self` |      | the supervisor's own host task as a hidden auto-adopted node: `start()` stamps its calling task into a per-graph `"supervisor"` node (`GRAPH.graph_ref.self_node()`), so waves/driver/monitor poll time is attributed instead of landing in the unsupervised share. No declaration needed; the node is outside the node table, so lifecycle never touches it (implies `trace`) |

**Only `macros` is on by default.** Everything the supervisor can *do* is opt-in,
including `control` and `pool`: both add code to the driver loop that runs every
iteration whether or not a graph uses it, so a graph with no pool and no control ops
should not carry either. `restart` and `bound-deps` enable `control` on their own, so
you rarely name it directly.

If you declare a `pool` without the feature, or call a control verb without it, the
macro and the compiler say so by name; the failure mode is a spanned error, not a
silent behaviour change.

## no_std / MSRV

`#![no_std]` and `#![forbid(unsafe_code)]`. Requires Rust 1.88+ (edition 2024). The embassy
dependencies are pre-1.0 (`embassy-executor` 0.10, `embassy-sync` 0.8, `embassy-time` 0.5), so a
consuming application must use compatible embassy minor versions. On a target without
atomic compare-and-swap, enable one of `portable-atomic`'s `critical-section` or
`unsafe-assume-single-core` features in the application.

## Full example

The [`firmware`](https://github.com/cedrivard/embassy-supervisor/tree/main/firmware) crate in the
repository is a complete working application on an RP2350: networking, an HTTP control plane, an
elastic worker pool, multi-executor tiers on both cores, trace observability, and OTA firmware
update, all driven by this supervisor.

## Migration

### 0.14.0 → 0.14.1

Ships with the same `embassy-supervisor-macros` 0.13.0 and `embassy-supervisor-syntax`
0.7.0. Purely additive: no code edit is needed. The new public items are
`Supervisor::run_with`, `RunPolicy`, `Origin`, `ReturnOnHalt`, `FailFast` and
`FaultKind::is_bring_up`. A crate that glob-imports `embassy_supervisor::*` together
with another crate exporting one of those names gets an ambiguity error where it uses
the name; import that name explicitly.

Behavioural change needing no code edit: `pool_pass` and `run_pools` log a failed pool
growth with `warn!`.

### 0.13 → 0.14

Ships with `embassy-supervisor-macros` 0.13.0 on `embassy-supervisor-syntax` 0.7.0 (both
pinned by exact version, no action needed). Each change below is a compile error at the
site, except the first, which changes what `run()` returns:

| 0.13.x | 0.14.0 |
|---|---|
| `Supervisor::run()` returned a bring-up or `Activate` fault | it logs the fault, reports it as `LifecycleKind::Faulted`, leaves the node down and enabled, and keeps driving; it returns only `ShutdownTimeout` or `RestartStorm`. Read bring-up faults from the log or a lifecycle sink |
| a `local` slot's `provide`, `take`, `restore` and `get` were safe | they are `unsafe`: call them on the executor the slot's declarations run on, inside `unsafe { }` with a `SAFETY` comment saying so |
| a hand-written `ResourceGate` implemented `filled_signal` | delete it: gate waits watch the shared gate event |
| a hand-built graph was a `Graph { nodes, topo, .. }` literal | `Graph::new(&NODES, topo)`, then `.with_pools(..)` and `.with_graph_ref(..)` where the features are on |
| the public `NodeCfg` fields `name`, `mode` and `spawn` were read directly | read the node instead: `TaskNode::name()` and `TaskNode::mode()`; `spawn` is internal |
| `Leased::is_drained()` | `Leased::is_closed()` |
| `match` on `CrumbPhase` without a wildcard | add a `_` arm (`#[non_exhaustive]`) |
| a target without compare-and-swap built without a `portable-atomic` feature | enable `portable-atomic/critical-section` or `portable-atomic/unsafe-assume-single-core` in the application |
| `#[dataflow]` below `#[embassy_executor::task]`, or on a method | put it above the task attribute, on a free fn |
| two items with one runtime name (`node Net` beside `node NET`, `pool W` beside `node W0`) | rename one |
| a `supervisor_fragment!` body setting `name:` or an `observe` default | move it to the composing graph |

Behavioural changes needing no code edit: `restart` brings back every member that was
running, including a `disabled` node started by hand and a running `OnDemand` node; a
parked `Pause` instance waits for its `ready` deps before it is resumed; a `Busy` spawn
gets one executor pass and then retries every 1 ms; `set_ready` and `clear_ready` wake
nobody when the flag does not change; `run()` logs and drops a control command for a
node outside its graph; a feature-gated construct under a `#[cfg]` on its item, entry or
clause compiles where the cfg is false.

### 0.12 → 0.13

Ships with `embassy-supervisor-macros` 0.12.0 on `embassy-supervisor-syntax` 0.6.0 (both
pinned by exact version, no action needed). A graph written with `supervisor_graph!`
needs no edit unless the parser now refuses it; each change below is a compile error at
the site:

| 0.12.x | 0.13.0 |
|---|---|
| a hand-built node was `TaskNode::new(&CFG, disabled)` | it is three statics: `static H: TaskHandle = TaskHandle::new();` and `TaskNode::new(&CFG, &H)`; a node that starts disabled says so on its config with `NodeCfg::with_disabled_at_boot(true)` |
| a hand-built `GraphRef::new(..)` carried the supervisor's self node under `trace-self` | add `.with_self_handle(&HANDLE)` over a `static HANDLE: TaskHandle` |
| `TaskNode::poll_observed_writes` was public | it is crate-private; the liveness monitor is its only caller |
| `with_beat_timeout` / `beat_timeout:` at or past `u32::MAX / 2` ticks was clamped | refused at compile time (about 35.8 min at 1 MHz) |
| the graph parser accepted a second `exit:` or `provides:`, `resources: []`, suffixed integer literals in clauses, non-`cfg` attributes on list entries and list entries without separators | each is a parse error naming the fix |

Behavioural changes needing no code edit: bring-up gate waits watch the shared gate event
rather than each slot's `Signal`; after each bind cascade the driver applies at most one
queued command; a `ready_on_write` node is probed at the capped 12.5 ms rate only for its
first `SLOT_READY_TIMEOUT`; a full health mailbox is retried by a later sweep.

### 0.11 → 0.12

Ships with `embassy-supervisor-macros` 0.11.0 on `embassy-supervisor-syntax` 0.5.0 (both
pinned by exact version, no action needed). Three breaking changes, each a compile error
at the site:

| 0.11.x | 0.12.0 |
|---|---|
| `state:` accepted any type | the type must be `Send`: the box crosses from the spawn glue into the resident shell through a handoff slot |
| `SpawnerSlot::set` took `&self` | it takes `&'static self`: call it on the slot `static`; on first fill it spawns the slot's executor fence onto that tier. A hand-built slot gets a fence with `SpawnerSlot::with_fence(..)`, which `supervisor_graph!` emits for every `executor NAME;` |
| `pool_size: N` on a node | removed, now a compile error naming the change: a resident `task:` shell never needs a second task slot. Delete the clause; a hand-written `spawn:` task fn declares its own `pool_size` |

Behavioural changes needing no code edit: `task:` shells are resident (spawned by the
first activation, parked between activations, woken in place by the next start);
`start_node` retries a `Busy` spawn until the node's `slot_timeout`; with
`restart-intensity`, `apply_control(ControlOp::Restart)` propagates
`FaultKind::RestartStorm`, so `run()` returns it.

### 0.10 → 0.11

Ships with `embassy-supervisor-macros` 0.10.0, unchanged (no action needed). One
signature changes, and the `unused_must_use` warning finds every call that ignores it:

| 0.10.x | 0.11.0 |
|---|---|
| `sup.activate(&N, &spawner).await` returned `()` | it returns `Result<(), NodeFault>`: the first startup fault of its best-effort wave, after the wave completes (successful starts are not rolled back). Handle it with `.await?` or a `match` |

Behavioural change needing no code edit: `apply_control(ControlOp::Activate)` returns that
fault too, so `run()` now also returns on a failed control `Activate`.

### 0.8 → 0.9

Only hand-written `Divisible` implementations change; macro graphs and `Budget` users need
no edit:

| 0.8.x | 0.9.0 |
|---|---|
| `Divisible::register` named no slot | it takes the slot to park on: `fn register(&self, slot: u8, waker: &Waker)`. `Budget` keeps one waker per slot and wakes only the slots whose grant changed |

### 0.7 → 0.8

Ships with `embassy-supervisor-macros` 0.9.0 on `embassy-supervisor-syntax` 0.3.0
(both pinned by exact version, no action needed). One signature changes, and the
compiler finds every site:

| 0.7.x | 0.8.0 |
|---|---|
| `node.open(&SIG).await` returned `&'static T` | it returns the gate's `Handle`: an `Open<T>` guard for `Backed`, which `Deref`s to the signal and leaves the reader count on drop. `let est = node.open(&SIG).await; est.receiver()` where `node.open(&SIG).await.receiver()` no longer lives long enough; `.signal()` where a `&'static T` is what you need (a `Leased` inside a `Backed`) |
| a hand-written `Gated` impl had `ensure` only | add `type Handle = &'static Self;` and `fn admit(&'static self) -> &'static Self { self }` (or a counting guard of your own) |
| a `#[dataflow]` body calling a node method named `retire` or `veto` | those are scanner verbs now (the producer's retirement, the writer's veto handle) and their first argument is rewritten to a `Sig` |

Everything else is additive: the `budget` and `veto` features, the `serialized` marker,
`Stamped`, the `public-gate` lint. A `Backed` gains 4 bytes and a `Signal`; a `Coupling`
gains 4 bytes under `veto` without `coupling-observe` (16 to 20 on thumbv8m) and none with it.

### 0.6 → 0.7

Ships with `embassy-supervisor-macros` 0.8.0, unchanged, so no action needed. No API
is removed and no signature changes; two control-plane behaviors are corrected.
`deactivate` no longer latches `disabled` on the target's transitive dependents:
they take the new `collateral` hold (`TaskNode::is_collateral()`), which every
automatic bring-up path honors the same way, and which `activate` on the ancestor
releases, restarting the released `Terminate`/`Pause` dependents in the same wave.
Code that read `is_disabled()` on a dependent to detect a subtree stop should read
`is_collateral()`; showing both flags distinguishes "deactivated in its own right"
from "held as a dependent". And an elastic pool now regrows without an app-side
`request_scale()` once its provider recovers: a readiness assertion, a provider
`restart`, `activate`, and the bound recovery path poke the pool driver
themselves, so a manual poke that papered over the parked driver is redundant
(and harmless).

### 0.4 → 0.5

Ships with `embassy-supervisor-macros` 0.7.0 (pinned by exact version, no action
needed). The largest breaking surface so far; the compiler finds every item:

| 0.4.x | 0.5.0 |
|---|---|
| every spawning verb (`start`, `run`, `run_pools`, `start_node`, `respawn_terminate`, `activate`, `apply_control`) took `Spawner` by value | they take `&Spawner`: `sup.run(&spawner)`. The `NodeCfg` spawn fn type still takes `Spawner` by value |
| lifecycle verbs returned `Result<(), ShutdownTimeout>`, `run` a `RunError`, bring-up a raw `SpawnError` | one `NodeFault { node, kind: FaultKind }` everywhere, with an unconditional `Display`; match `fault.kind` against `ExecutorSlotEmpty`, `ResourceMissing`, `ReadyDepTimeout { dep }`, `Spawn(SpawnError)`, `ShutdownTimeout` |
| `Graph<N>` / `Supervisor<N>`, edges read off `GRAPH.deps` / `GRAPH.order` | a `Topology` parameter (annotate with the macro's alias, `Supervisor<5, GRAPH_TOPOLOGY>`) and edges read through `GRAPH.deps_of(i)` / `GRAPH.order()` |
| `TaskNode` carried `name`/`mode`/`spawn` fields and the `with_*` builders | flash `NodeCfg` + RAM handle: read `name()` / `mode()`; the builders live on `NodeCfg`; a hand-built node is three statics: the flash `NodeCfg`, the `.bss` `TaskHandle` and the `TaskNode` that pairs them (macro graphs are unaffected) |
| default features `["control", "pool", "macros"]` | just `["macros"]`: name `control`/`pool` explicitly where used (`restart`/`bound-deps` still pull `control` in; a `pool` without the feature is a spanned macro error) |
| `trace::register_graph(&NODES)`, capped at 4 graphs | `trace::register_graph(GRAPH.graph_ref)`, no cap; only hand-registered graphs are affected, `Supervisor::start` registers for you |
| exhaustive `match` on `ControlOp` | the enum is `#[non_exhaustive]` (and gains `Restart` under the `restart` feature) |
| sequential bring-up and teardown | concurrent waves, order guarantees intact. The contract to re-check: an **unordered** node may be told to stop while others still run; a node publishing a handle to consumers it has no edge from holds its own shutdown with `Leased` + `drain`; whatever a node uses *during* its shutdown must be one of its `deps:` |
| gate waits budgeted 100 ms per gate | one `slot_timeout` budget per node in the `start()` wave; the single-node verbs still bound each gate |

Behavioural changes needing no code edit: `clear_ready()` stays status-not-control
unless an edge opts in with `bound`; the monitor wakes per earliest-due node instead
of on a global `min(beat_timeout)/2` period; resources no longer cross the spawn
call, so a failed claim cannot drop a `lend`/`consume` value.

### 0.4.0 → 0.4.1

Ships with `embassy-supervisor-macros` 0.6.0 (pinned by exact version, no action
needed). Purely additive at run time, with one source-level catch: `exit:` declared on
a worker that can never return is now [a compile error](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/dsl/index.html#limits-and-compile-time-validation)
instead of a slot nothing ever filled. If a graph hits it, drop the `exit:` clause;
that worker never produced a value in the first place.

### 0.3 → 0.4

Ships with `embassy-supervisor-macros` 0.5.0 (pinned by exact version, no action needed).
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
| explicit `SLOT.ready().await` before `start()` | no longer needed: `start` awaits each `executor:` node's slot itself |

### 0.1 → 0.2

| 0.1.x | 0.2.0 |
|---|---|
| `task_graph! { &A, &B }` | `supervisor_graph! { node A = ...; node B = ...; }` |
| `Supervisor::new(&ALL_NODES, &DEPS, ORDER)` | `Supervisor::new(&GRAPH)` |
| `.with_pools(POOLS)` | gone: pools ride in `GRAPH` |
| `NODE_COUNT` | `GRAPH.nodes.len()` |

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
