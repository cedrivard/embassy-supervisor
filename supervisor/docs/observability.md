# Observability

*(feature family `trace`, all opt-in)*

embassy-executor ships raw `_embassy_trace_*` instrumentation hooks that identify tasks only
by an opaque `u32`. The `trace` feature makes the supervisor their batteries-included
consumer: the generated spawn glue captures each `SpawnToken`'s id into its node, so every
executor poll is attributed to a *named* node, correctly across respawns.

- **Per node**: accumulated poll time (`exec_ticks`), poll count, and the longest single
  poll ever (`max_poll_ticks`): the "never yields" watermark that names a task that hogged
  its executor, even after the fact.
- **Per executor**: a full time decomposition via `trace::executor_stats`: idle, in-poll
  (every task poll, supervised or not), and by subtraction the **executor overhead**
  (scheduler bookkeeping + hook cost + ISRs between polls) and the unsupervised-task
  share; plus poll/pass counters and the in-flight poll (`trace::current_task` /
  `trace::stalled_task(executor, threshold)` for live blocked-task detection; their docs
  say which contexts can ask).
- Counters are wrapping `u32` ticks: sample twice, `wrapping_sub`, divide. The in-repo
  firmware's README covers how to read the numbers in practice (CPU%, busy% vs overhead,
  polls-per-pass as a wake-storm tell).

The split across the family: `trace` is recorders only; `trace-hooks` additionally emits the
hook definitions at the graph declaration site (exactly one set may exist per binary;
define your own hooks and forward to the `trace::on_*` recorders if you need custom ones); `metadata-names` stamps node names into task Metadata for external tooling
(SystemView, debuggers); `trace-names` is shorthand for `trace` + `metadata-names`;
`trace-nested` makes accounting preemption-exact: a nested higher-tier poll credits its
time back to the window it interrupted (register `trace::set_core_id_fn` on multi-core for
one preemption stack per core).

`metadata-names` is independent of `trace`: it pulls only `embassy-executor/metadata-name`,
not `embassy-executor/trace`, so it emits **no** `_embassy_trace_*` hook symbols and links
cleanly on its own. That makes it the piece you want for a pure external tracer: enable
`metadata-names` alongside embassy's own `rtos-trace` feature (which also pulls
`metadata-name`) and SystemView shows your graph's node names, with none of the supervisor's
recorder overhead and no hook-symbol requirement. Enabling `trace`/`trace-names` instead
brings the recorders back and, as ever, requires the hook symbols (`trace-hooks` or your own).
The stamp is the node name plus a trailing NUL, since SystemView reads task names as C
strings; `TaskNode::name()` stays without it.

Limitations: accounting is preemption-naive without `trace-nested`; hardware-ISR time is
invisible either way; executor busy% exceeds the per-node sum by a per-poll accounting gap
(`ExecutorStats` measures it as `busy − in-poll`); at most 4 executors are tracked. Parked /
closure-spawned nodes register with one call: `TaskNode::adopt(&token)`, or
`node.adopt_current().await` from inside the task body, when nobody holds the token. The
supervisor's own host task is part of the unsupervised share unless `trace-self` is on:
each graph then carries a hidden `"supervisor"` node that `start()` adopts as its calling
task, attribution being task-granular (everything else that task polls is billed to it).
The macro gives that node a `.bss` handle of its own; a hand-built `GraphRef` adds the
node with `GraphRef::with_self_handle(&HANDLE)`.

The hook API is an executor implementation detail, and this feature tracks the executor
minor version the crate pins. It has already changed shape past 0.10: embassy's git main
(the next release) replaces the seven `_embassy_trace_*` symbols with a `raw::trace::Trace`
impl registered through `trace_impl!`, and passes `ExecutorId`/`TaskId` newtypes instead
of `u32`s. Building against that executor takes `RUSTFLAGS="--cfg
embassy_supervisor_trace_v2"` (with `[patch.crates-io]` pointing `embassy-executor` **and**
`embassy-executor-timer-queue` at git; the two must move together, or embassy-time's queue
utils size `TimerQueueItem` through a second copy and timeouts stop firing). The recorders
keep their `u32` keys either way; custom hooks narrow ids with `trace::task_key` /
`trace::executor_key`. CI's canary job builds and tests exactly that configuration, and the
cfg goes away once the crate pins the release that ships it.

# Lifecycle events and the bring-up breadcrumb

### Lifecycle events (features `lifecycle-events`, `lifecycle-sink`)

Every transition a supervisor drives (the waves of `start`/`teardown`/`activate`/
`deactivate`/`restart`/`respawn_terminate`, the single-node verbs, pool scaling, bound-dep
cascades) posts one `LifecycleEvent { node, kind }`:

| `LifecycleKind` | when |
|---|---|
| `Started` | a fresh instance was spawned (an app-spawned node: marked running) |
| `Resumed` | a parked `Pause` instance was resumed in place |
| `StopRequested` | the node was signalled to stop |
| `Stopped` | it acked without its body returning (a parked `Pause` instance, a hand-written `ack_dropped()`), or its body returned but a preempting supervisor saw the ack before `mark_exited` (see below) |
| `Exited` | it acked by its body returning (`mark_exited`, which every `task:` shell calls), as seen by a supervisor that cannot preempt the worker |
| `Faulted(FaultKind)` | a lifecycle step failed: gate timeout, spawn failure, missed ack, refused restart |

The supervisor tells `Exited` from `Stopped` by the node's completion record at the
moment the ack resolves. A body that acks by returning acks inside its last poll, and the
shell records the completion right after it in the same poll, so a supervisor on the
worker's executor (or on a lower-priority tier of the same core) always sees both. A
supervisor on a higher-priority tier or another core can run between the two and post
`Stopped` for that stop.

```rust,ignore
loop {
    let ev = embassy_supervisor::wait_lifecycle().await;
    info!("{} {}", ev.node.name(), ev.kind.as_str());
}
```

The queue holds 8 and is lossy on purpose: the waves run on
the supervisor task, which must never park behind a slow consumer. While it is full
new events are dropped and counted (`lifecycle_events_dropped()`), so a consumer that
starts after a large bring-up sees its head, not its tail. One consumer;
`try_wait_lifecycle()` for polling. A body that returns on its own, unasked, is not
evented, since no supervisor is watching it then; `has_exited()` reads it. Costs one 8-slot
channel, one `AtomicU32`, and one `try_send` per transition.

An app that only logs or records events does not need the queue or a task to drain it.
`lifecycle-sink` delivers the same events by calling a function instead:

```rust,ignore
fn record(ev: embassy_supervisor::LifecycleEvent) {
    let now = embassy_time::Instant::now(); // the transition's own time
    info!("{} {}", ev.node.name(), ev.kind.as_str());
    // ... push (ev.node, ev.kind, now) into a ring ...
}
embassy_supervisor::set_lifecycle_sink(record); // before the first wave
```

The sink is called synchronously, in transition order, from the supervisor task inside
the wave or verb that made the transition, so nothing is dropped and a timestamp taken
in it is exact. That is also its constraint: it runs on the supervisor's executor (an
interrupt executor or another core in a tiered app) and must be short and
non-blocking: a log line or a critical-section ring write. Events before
`set_lifecycle_sink` are not delivered. Costs one fn pointer and one critical section
per transition. Both features can be on; then every event reaches both.

On the demo firmware (RP2350, thumbv8m), moving from the queue plus a draining
`watchdog` branch to the sink cut the feature's `.bss` from +380 B to +204 B
(the queue's 128 B and 48 B of the consumer's future) and its flash by 424 B; the
rest is the firmware's own 16-entry ring.

### Bring-up breadcrumb (feature `breadcrumb`)

A reset mid-bring-up leaves nothing to say which node hung it. With `breadcrumb` each
node records the last phase a supervisor drove it through (`node.crumb()`:
`Idle`, `Spawning`, `Started`, `StopSignalled`, `Stopped`, `Faulted`, `Resumed`), and
`crumb_word()` packs the binary-wide last transition into one `u32`: bit 31 valid,
bits 16..24 the phase, bits 0..16 the node's slot in `GRAPH.nodes`. Stash the word
somewhere that survives the reset, decode it on the next boot:

```rust,ignore
// in the watchdog feeder (or the panic handler): one store per pass
pac::WATCHDOG.scratch(6).write_value(embassy_supervisor::crumb_word());

// at boot
if let Some((phase, slot)) = embassy_supervisor::decode_crumb(prev) {
    let name = GRAPH.nodes.get(slot as usize).copied().flatten().map_or("?", |n| n.name());
    warn!("previous run ended at {} {}", name, phase.as_str());
}
```

A wave parked on a gate leaves `Spawning`; a teardown that never got its ack leaves
`StopSignalled`. The word is last-writer-wins across graphs and concurrent waves, and a
multi-graph binary cannot tell which graph's slot it names; the per-node `crumb()` dump
(`GRAPH.nodes.iter().flatten().map(|n| (n.name(), n.crumb()))`) is authoritative. Relaxed
stores throughout: a diagnostic, not a synchronisation point. Costs one `AtomicU8` per
node, one `AtomicU32`, and one store pair per transition.

# Fault injection

`fault-inject` lets tests or dashboards inject faults into a node without worker
cooperation:

```rust,ignore
HEARTBEAT.inject(Fault::Stall)?;                       // stop polling the worker
HEARTBEAT.inject(Fault::Wedge)?;                       // hide stop, swallow ack
HEARTBEAT.inject(Fault::Crash)?;                       // drop the future, record exit
HEARTBEAT.inject(Fault::Hog(Duration::from_secs(3)))?; // busy-spin for 3 s
HEARTBEAT.clear_fault();                               // replay what was withheld
```

| verb | effect | supervisor sees | recovery |
|---|---|---|---|
| **stall** | `task:` shell stops polling the worker | `Stale`, zero trace polls | `clear_fault` or restart |
| **wedge** | hides stop request and swallows ack | `ShutdownTimeout`, node stays running | `clear_fault`, then the pending verb completes |
| **crash** | drops the future, runs normal exit path | node reads as exited | restart |
| **hog** | after 250 ms grace, busy-spins for the bound | every node on that executor stalls | the bound; longer than the watchdog feed is a reset |

Stall, crash, and hog only work for `task:` shells; `spawn:` tasks return
`InjectError::NoShell`. Wedge works everywhere. Adds one `AtomicU8`, one `AtomicU32`,
and one `AtomicWaker` per node. The [demo firmware](https://github.com/cedrivard/embassy-supervisor/blob/main/firmware/README.md#exercise-the-supervisor) exposes the verbs over HTTP.
