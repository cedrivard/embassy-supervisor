# Recipes by use case

### Heap and the graph

The reclaimability boundary, stated plainly: **task storage stays static, by
soundness, not preference.** Every `Waker` embassy hands out is an unrefcounted raw
pointer into the task's storage; stale wakes against *reused* storage are safe no-ops,
against *freed* storage they are use-after-free, and nothing counts outstanding waker
clones, so no safe free point exists. What IS reclaimable is
**future-owned state**: everything the body owns drops when it returns (embassy never
force-cancels: after the shutdown select, the future runs to completion). The
supervisor therefore uses heap only where it comes back:

- **`state: Type = init_expr`** *(feature `heap-state`)*: per-activation boxed state.
  The spawn glue fallibly boxes the init value (**alloc failure = `FaultKind::Spawn`**,
  nothing spawned or stranded, retry when heap frees up), the shell lends the worker
  `&mut Type` (after resources, before extras; e.g. a node with
  `resources: [STACK: shared u32], state: Buf = Buf::new()` has a worker
  `async fn w(node: &'static TaskNode, stack: u32, buf: &mut Buf)`), and the Box
  **drops when the activation ends**, before restores and the completion record, so
  `has_exited()` implies the heap is back. Every activation allocates fresh; N
  respawns = net zero. On a pool, each member boxes its own. The container (the
  future's static storage) is now a thin shell; the bulk is paid only while the phase
  runs. The feature's whole `unsafe` surface is two small fallible-boxing helpers
  emitted into YOUR crate (the `local-resources` precedent), one per `state:` form the
  graph uses; you need a `#[global_allocator]`.
- **`state: zeroed Type`**: the same lifecycle with no init value: the glue calls
  `alloc_zeroed` and hands the worker the block as a live `&mut Type`. With
  `= init_expr` the value is built in the spawner's frame and copied into the Box
  unless the optimizer elides it, so a large buffer set briefly costs its size in
  stack; the zeroed form never does, at any size or opt-level. `Type` must implement
  [`Zeroable`](https://docs.rs/bytemuck/latest/bytemuck/trait.Zeroable.html)
  (re-exported as `embassy_supervisor::Zeroable`): `unsafe impl Zeroable for Bufs {}`
  for a struct of byte arrays, or bytemuck's derive. That impl is your soundness
  promise: the zeroed helper hands the zero-filled block to the worker as a live `Type`.
- **`consume Box<T>` slots**: the app-provided variant, zero crate support needed:
  provide a fresh `Box` before each activation, the worker owns it, drop-on-exit
  frees it, the slot stays empty until re-provided (fail-closed respawn). Use it when
  the *app* decides the allocation (budget checks before an `Activate`; pair with a
  free-bytes gate); use `state:` when the graph should just do it.
- **Lend a `Box`** (`RES: Box<Big>` with the default kind) to keep ONE allocation
  alive across respawns instead: pay-once, reclaim-never, but no per-cycle churn.

`Box<T>` in any of these still requires `T: 'static`: placement recipes, not
lifetime escape hatches.

### Subordinate sub-graph under an app state machine

The graph does not have to own your `main`. A state-machine (or super-loop) firmware
keeps owning sequencing (and carries data between states, which a declarative graph
cannot) while a **dedicated named sub-graph** is cycled with whole-graph ops per
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
ack, and the next `start()` waits for each resident `task:` shell to park (and each
`spawn:` task to return, see [the executor fence](crate::guide::executors#respawning-across-tiers-resident-shells-and-the-executor-fence)), so re-entering the
state cannot race the previous instances; `consume` slots make "rebuild the radio each
entry" fail-closed instead of stale-reuse. This relies on a `spawn:` task fn having no
free pool slot; a pooled `spawn:` fn must ack as its last use of the node. `task:` nodes
and `task:` pools carry no such rule.

**One-graph variant** *(feature `control`)*: declare the subtree `Terminate` +
`disabled` in the **main** graph and drive it as a dependency cascade: `Activate` on
the **leaf** pulls its transitive deps up in topo order (skipping already-running
ones), `Deactivate` on the **root** tears its transitive dependents down in reverse.
Prefer this over a separate graph when the subtree **depends on always-on nodes**
(graphs are closed worlds; there are no cross-graph dep edges), when the subtree
should ride the system-wide sleep/wake lifecycle (one `teardown()` covers it, and the
`disabled` latch keeps it down across the wake's `respawn_terminate()`), or when the
phase must be drivable from anywhere via `request_control` through the shared mailbox
(e.g. the supervisor lives inside a `run()` driver task):

```rust,ignore
State::Upload => {
    WIFI_HW.provide(build_wifi(&mut ctx));
    sup.activate(&UPLOAD, &spawner).await?;           // WIFI -> NET -> UPLOAD
    let next = upload_screen(&mut ctx).await;
    sup.deactivate(&WIFI).await?;                    // UPLOAD -> NET -> WIFI
    next
}
```

(`activate`/`deactivate` are the cascading, `disabled`-latching verbs; contrast the
single-node, no-cascade `start_node`/`stop_node`. `apply_control` is the same pair
routed through the `request_control` mailbox, for code that doesn't hold the
supervisor.)

Either way the supervisor is a library here, not the owner of bring-up.

Node and pool names below are invented; swap in your own worker fns. They use `task:`
throughout (the preferred form: plain async fns, no `#[embassy_executor::task]`);
substitute `spawn:` in the same position for any of the four cases in
[`spawn:` vs `task:`](crate::guide::dsl#spawn-vs-task-which-to-use). Nodes shown without either are
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
time; a cycle or an unknown dep name is a compile error.

### Generic worker over N driver types (`task:`)

```rust,ignore
// ONE generic worker, a plain async fn, not a #[embassy_executor::task]:
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

Args (`bme()`, `sht()`) are evaluated inside each shell at the start of every
activation, on the node's own executor.

### Provider node (async multi-output construction in the graph)

One async bring-up often builds SEVERAL correlated driver objects (a cyw43 radio:
two runners + a `Control` + a `Stack` handle) that different nodes consume, and must
re-run every wake cycle. That builder becomes an ordinary **provider node**: no
special DSL, just the gate machinery pointed at runtime provisioning:

```rust,ignore
// The provider: builds and provide()s, holds NOTHING afterwards. Terminate
// mode makes respawn_terminate re-run the build each wake cycle.
async fn radio_hw(node: &'static TaskNode) {
    let (runner, control, stack) = build_radio().await;  // hundreds of ms
    // SAFETY: called on the slots' executor: the graph holds this provider and
    // every declaration of the three `local` slots to one.
    unsafe {
        RUNNER.provide(runner);     // consume slot: empty again after teardown
        CONTROL.provide(control);   // consume slot
        STACK.provide(stack);       // shared slot: fanned out, stays filled
    }
    node.wait_shutdown().await; // returning acks via the shell's mark_exited
}

supervisor_graph! {
    // `provides:`: these slots die with this node: its
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
consumers first (reverse topo: `consume` values are dropped, `shared` handles just
die with their copies) and the provider last; `respawn_terminate` re-runs the
provider FIRST, so the consumers' gate waits rendezvous with the freshly built
values. A provider that dies before providing surfaces as `FaultKind::ResourceMissing` on its
consumers after their `slot_timeout`: fail-closed, never a stale reuse.

What the clause clears, and why the `shared` slot needs it, is under
[`provides:`](crate::guide::dsl#provides-slots-that-die-with-their-producer). The consumers declare
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
    // (a link-loss handler would clear_ready(), which is status, not control:
    // already-running dependents keep running; future spawns and pool growth wait)
}
```

`set_ready()` latches until `clear_ready()` or the pre-spawn reset (a respawned
provider re-asserts for its new instance). The wait is bounded by the DEPENDENT's
`slot_timeout:` and fails the spawn with `FaultKind::ReadyDepTimeout`, so a provider that never
becomes ready is a loud, retryable error, never a hang. A `bound` edge is the
exception: there the spent budget PARKS the dependent (`BOUND_STOPPED`) instead of
faulting, and the bind loop lifts it when the provider asserts (defer until serving).

`ready` carries a state, not a value. When what the dependent waits for *is* something
the provider hands over (a stream, a stack handle), a `resources:` slot filled under
`provides:` is the rendezvous and `ready` adds nothing; see the provider-node recipe
above and [Resource or signal?](crate::guide::dsl#resource-or-signal).

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

A `Pause` node acks a shutdown, then parks on `wait_resume()`; the I2C handle it holds is
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

// Plain async fn; `task:` stamps the #[embassy_executor::task] shell:
async fn log_drain_worker(node: &'static embassy_supervisor::TaskNode) {
    node.set_detached(true); // full hands-off from here on
    loop { /* drain forever, self-managed */ }
}
```

After `set_detached(true)` the supervisor never drives the node again: teardown, control
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

`SAMPLER` runs at raised priority while `LOGGER` stays on the thread executor, yet the
dependency between them is still honored. `sampler_worker`'s future may hold whatever it
likes (the `Send` bound is on the shell's arguments, not the future); if the
slot is never filled, `start()` fails with `FaultKind::ExecutorSlotEmpty` after a bounded wait. A
`task:` extra is evaluated inside the shell, i.e. on the raised-priority tier at the start
of each activation; switch that node to `spawn:` when an argument must instead be snapshotted on the
supervisor's executor at the moment of the spawn (case 4 of
[`spawn:` vs `task:`](crate::guide::dsl#spawn-vs-task-which-to-use)).

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
`min: 0` lets the policy stop every `OnDemand` member; `DeferredShrink` keeps one idle
spare, so a pool on it settles at one member.

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
detaching is still what makes it run once **ever**; without it, `respawn_terminate` on
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

static SUP: Supervisor<5, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);

// A parked node (neither `task:` nor `spawn:`): main spawns it by hand because it
// needs a value only main has, here the `Spawner` that `respawn_terminate` takes:
//     spawner.spawn(power_task(&POWER, spawner).unwrap());
#[embassy_executor::task]
async fn power_task(node: &'static embassy_supervisor::TaskNode, spawner: Spawner) {
    node.set_detached(true); // survives the teardown it is about to drive
    loop {
        wait_for_idle().await;
        if let Err(fault) = SUP.teardown().await {  // quiesce the graph; POWER is skipped
            // after an `Err` the graph is partially down: escalate, do not sleep on it
            escalate(fault).await;
        }
        enter_low_power().await;                    // Pause nodes stay parked
        SUP.resume_pausable();                      // thaw the parked diagnostics
        if let Err(fault) = SUP.respawn_terminate(&spawner).await {  // respawn the stateless services
            escalate(fault).await;
        }
    }
}
```

The common shapes combined: a latency-critical node on an interrupt tier, a `Pause`
diagnostics node that keeps its state across the sleep, and a detached coordinator that
drives the whole sleep/wake cycle itself; because it's detached, its own `teardown()` and
`respawn_terminate()` calls skip it.
