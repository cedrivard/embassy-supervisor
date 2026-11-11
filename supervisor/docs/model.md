# The model

Three pieces, all `static`:

- **`TaskNode`**: one per managed task, emitted by the macro as three statics: the flash
  `NodeCfg`, the `.bss` `TaskHandle` and the `TaskNode` that pairs them. The `NodeCfg` is
  the immutable half (name, mode, spawn fn, gates, budgets, coupling tables, the
  `disabled` marker); the `TaskHandle` is the live half (atomic flags + signals, all zeros
  at rest). The node itself is two pointers, so it stays in flash and the atomics cost
  RAM only. Read the declared side through methods (`name()`, `mode()`,
  `slot_timeout()`, ...). The *task side* of the protocol is a handful of node methods;
  see [Writing supervised tasks](crate::guide::tasks).
- **`Graph<N, T>`**: the macro-emitted `GRAPH`, with `nodes` (fixed `[Option<&TaskNode>; N]`;
  a `#[cfg]`-ed-out node keeps its slot as `None`), `topo` (the `Topology`: per-node
  dependency indices + the compile-time topological order, or the zero-sized `Flat`
  when the graph declares no `deps:` at all), and `pools` (with the `pool` feature).
  Read the edges through `deps_of(i)` / `dependents_of(i, ..)` / `order()`. The
  topology also carries the graph's structural **shape** bits (which modes, gates,
  markers and pools the graph contains, decided at expansion), so the lifecycle code
  serving a structure the graph lacks is compiled out rather than branched over.
- **`Supervisor<N, T>`**: construction-free orchestration over `&GRAPH` (`new` is
  `const`; the macro emits a `GRAPH_TOPOLOGY` alias for `T`, so
  `static SUP: Supervisor<5, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);` works; `N` =
  total graph slots, pool members included, the same `N` as `Graph<N, T>`), in three
  tiers: whole-graph, single-node, and cascading subsystem verbs.

The full verb surface, with signatures (every error type is `Debug`, so `.unwrap()` /
`.expect()` work, and `defmt::Format` under the `defmt` feature):

| verb | signature | feature |
|---|---|---|
| `start` | `async fn(&self, &Spawner) -> Result<(), NodeFault>`: quiescent → running, any state | |
| `run` | `async fn(&self, &Spawner) -> NodeFault`: `start` (best-effort) + the driver loop; returns only on a missed shutdown ack or a refused restart | `control` or `pool` |
| `teardown` / `teardown_continue` | `async fn(&self) -> Result<(), NodeFault>` | |
| `respawn_terminate` | `async fn(&self, &Spawner) -> Result<(), NodeFault>`: wake pair, with... | |
| `resume_pausable` | `fn(&self)`: ...this (sync: parked tasks pick up immediately) | |
| `start_node` | `async fn(&self, &'static TaskNode, &Spawner) -> Result<(), NodeFault>`; a running or detached node is left alone (`Ok`) | |
| `stop_node` | `async fn(&self, &'static TaskNode) -> Result<(), NodeFault>`: awaits the ack | |
| `resume_node` | `fn(&self, &'static TaskNode)`: sync, `Pause` nodes only | |
| `activate` | `async fn(&self, &'static TaskNode, &Spawner) -> Result<(), NodeFault>`: best-effort cascade; returns the first startup fault | `control` |
| `deactivate` | `async fn(&self, &'static TaskNode) -> Result<(), NodeFault>`: cascade | `control` |
| `apply_control` | `async fn(&self, ControlCommand, &Spawner) -> Result<(), NodeFault>` | `control` |
| `restart` | `async fn(&self, &'static TaskNode, &Spawner) -> Result<(), NodeFault>`: cycle the node and its transitive dependents | `restart` |
| `apply_bind` | `async fn(&self, &Spawner) -> Result<(), NodeFault>`: stop or bring back `bound` dependents after a readiness change | `bound-deps` |
| `run_pools` | `async fn(&self, &Spawner) -> NodeFault`: completes only on error | `pool` |
| `pool_pass` | `async fn(&self, &Spawner) -> Result<Option<Instant>, NodeFault>`: one pools pass | `pool` |
| `monitor` | `async fn(&self) -> Infallible`: the liveness sweep behind `wait_health()`; never returns | `liveness-monitor` |

Error provenance: every lifecycle failure is one `NodeFault { node, kind: FaultKind }`
(`ExecutorSlotEmpty`, `ResourceMissing`, `ReadyDepTimeout { dep }`, `Spawn(SpawnError)`,
`ShutdownTimeout`, or with `restart-intensity` `RestartStorm`) with an unconditional
`Display` that names the node and the cause, so `{fault}` is enough for an escalation
message. `Aborted`, `Resumed`, `ControlQueueFull` (feature `control`) and `InjectError`
(feature `fault-inject`) are the other crate error types; `SpawnError` is re-used from
`embassy_executor` and appears only inside `FaultKind::Spawn`. All the guarantees here
are cross-thread (release/acquire atomics); a host test's main thread reads them as
safely as another task.

The control mailbox (feature `control`) is two free functions and two small types:
`async fn request_control(&'static TaskNode, ControlOp)` (lossless: awaits mailbox
capacity), `fn try_request_control(..) -> Result<(), ControlQueueFull>` (sync
contexts), and `enum ControlOp` with `Activate`, `Deactivate`, and (feature `restart`) `Restart`;
the enum is `#[non_exhaustive]`. Higher-level verbs (start/stop/pause/resume) fold
onto the first two per the node's `Mode`.
All of these are importable from the crate root (`embassy_supervisor::try_request_control`,
`embassy_supervisor::ControlOp`, …); a `NodeFault`'s fields are
`pub node: &'static TaskNode` and `pub kind: FaultKind`.

Mailbox acceptance is not execution success: `request_control` waits only until the
command can be queued, and `try_request_control` reports only queue capacity.
`apply_control` propagates Activate's startup fault; the `run` driver logs it, reports it
as a `Faulted` lifecycle event, and keeps serving the mailbox. A node whose bring-up
faulted, at boot or on an `Activate`, stays down and enabled until the next `Activate`
retries it. Restart keeps its distinct control policy: `apply_control`
returns its shutdown timeout but logs and absorbs its startup faults; a direct
`restart` call returns those faults. With `restart-intensity`, a refused restart
(`FaultKind::RestartStorm`, see [`restart_budget:`](crate::guide::dsl#restart_budget-restart-intensity))
is propagated by `apply_control` too: a crash loop is a fault the app must see.

activate walks up through dependencies; deactivate walks down through dependents. They are opposites, but they compose symmetrically over a subtree.

`activate` completes its best-effort startup wave before returning the first
`NodeFault` encountered, or `Ok(())` if no startup fails. It does not roll back
successful starts: an error can leave a partially running graph. Already-running
nodes are skipped, and an activation that selects no work succeeds. Handle the
result with `.await?` or explicit error handling.

A deactivate call marks only the seed node as disabled. The seed is the target node, or the whole pool if the target is a pool member. Dependents are marked as collateral instead. Collateral blocks automatic bring-up just like disabled, but `activate` clears it once no disabled node remains anywhere in the dependent's transitive dependencies. Any Terminate or Pause dependents released this way restart in the same wave.

Overlapping deactivations compose cleanly. A node below two deactivated ancestors restarts on the second `activate`. A node deactivated directly keeps its latch through an ancestor's cycle. A manual `start_node` overrides the hold, but the override only clears at spawn entry, so a node that started while a hold was being latched still runs flagged and releasable instead of coming up silently unheld.

Released OnDemand pool members are not started immediately. They regrow through demand from the elastic policy or a gated read, and `activate` prompts the pool driver to re-check standing demand. A pool deactivated directly still needs its own Activate later. Targeting any member expands to the whole pool, since membership is part of the seed.

`Mode` decides what each transition does to a node:

| mode | at boot | on teardown | on bring-up |
|---|---|---|---|
| `Terminate` | spawned | exits its loop (acks) | **respawned** (`respawn_terminate`) |
| `Pause` | spawned (or app-spawned if parked) | acks, then parks on `wait_resume()` | **resumed in place** (`resume_pausable`), keeps held resources |
| `OnDemand` | not started | stopped like `Terminate` | not auto-started: pools/control start it |

How a task implements its half of these transitions is the
[TaskNode API](crate::guide::tasks).

# Lifecycle reference

The canonical per-operation matrix: what each supervisor operation does to a node, by mode
and by the three lifecycle-spanning flags (`disabled`, `collateral`, `detached`). Other docs link here.

**Missed acks are errors, not panics.** Every stop path awaits the target's ack with a
2 s timeout (per-node override: `ack_timeout:`); a node that misses it is returned as a
`NodeFault`
(`FaultKind::ShutdownTimeout`) naming the node,
from `stop_node`, `teardown`, and (feature `control`) `apply_control`; `run_pools`
completes (only) with it, and `pool_pass` returns it, when a shrink hits a wedged member.
`teardown` **aborts at the first timeout** so a still-live dependent never has its
dependencies stopped under it;
after `Err` the graph is partially down and the application escalates (hardware watchdog
reset, `panic!`, or retry). `teardown_continue` is the best-effort variant for the
"hardware reset next" path: it visits every remaining node past the wedge and reports
the first timeout at the end.

| operation | `Terminate` | `Pause` | `OnDemand` | disabled | detached |
|---|---|---|---|---|---|
| `start` *(boot + re-entry, async)* | spawned in dep order; already-running skipped (idempotent) | spawned (cold); an instance parked by an earlier `teardown` is **resumed in place**; a parked (no-`spawn:`) node is only marked running | skipped | skipped | first start spawns it (tasks detach *themselves* after that); re-entry skips it, since its instance survived the teardown |
| `teardown` | shutdown + ack, exits | shutdown + ack, parks on `wait_resume()` | stopped if running, else skipped | already down, nothing to do | **skipped** (self-managed) |
| `deactivate` *(control)* | seed: disabled + stopped; transitive dependents: `collateral` + stopped, dependents first | disabled (or `collateral`) + stopped, parks; stays parked | disabled + stopped (the whole pool, atomically, when a member is the target); `collateral` as a dependent | re-disabled (idempotent) | **skipped**: never pulled into the cascade, even when targeted directly |
| `activate` *(control)* | enabled + started, after its transitive deps; a `collateral` dependent with no disabled dep left is released + restarted in the same wave | enabled + resumed in place | enabled/released only: the pool policy regrows it under load (the wave pokes the pool driver) | this is the flag it clears | **skipped**: not re-enabled, not restarted; its `deps:` are start-ordering only and are not expanded |
| `stop_node` | shutdown + ack | shutdown + ack, parks (**this is the single-node pause**) | shutdown + ack (the pool-shrink path) | not running → no-op | **no-op** |
| `resume_node` | no-op (wrong mode) | reset + resumed in place, keeps held resources | no-op (wrong mode) | skipped: a manual pause sticks | **no-op** |
| `respawn_terminate` *(async)* | reset + respawned in dep order | untouched (use `resume_pausable`) | left down; the policy regrows it | skipped: a manual stop sticks | **skipped**: it never went down, respawning would double-spawn |
| `resume_pausable` | untouched | reset + resumed in place, keeps held resources | untouched | skipped: a manual pause sticks | **left parked** |

Three flags cut across the modes:

- **`disabled`** is the "a human said stop" latch: `deactivate` sets it (on its seed),
  `activate` clears it, and every bring-up path honors it so a manual stop/pause survives a
  wake respawn or an elastic regrow. Its companion **`collateral`**
  (`TaskNode::is_collateral()`) marks a node stopped only as a *dependent* of a deactivated
  node; the bring-up paths honor it the same way, but `activate` on the ancestor releases
  it.
- **`detached`** (`TaskNode::set_detached(true)`) is full hands-off: the node manages its own
  lifecycle and the supervisor never drives it again. Its `deps:` still order its *first*
  spawn; after that, the graph only remembers where it was declared.

**Defaults in one place:** shutdown-ack timeout **2 s** (override with `ack_timeout:`
for a node whose cleanup legitimately takes longer; a missed ack returns
`FaultKind::ShutdownTimeout`, from the moment that node is signalled on either stop
path); pre-spawn gate budget **100 ms** per node: the
`start()` wave covers all of a node's gates (executor slot, resources, ready
deps) with one budget from when its in-pass deps resolve, while the single-node
verbs bound each gate separately (override with `slot_timeout:`; timeout = a
`NodeFault` naming the gate); control mailbox depth **4** (`request_control`
awaits capacity, `try_request_control` reports `ControlQueueFull`); trace
registries track up to **4** executors and any number of graphs (an intrusive
chain through each graph's `GraphRef`).
