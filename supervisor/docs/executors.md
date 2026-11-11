# Multi-executor tiers and multi-core

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
remote nodes, and a whole `pool` can carry `executor: CORE1`: an elastic worker pool
on core 1, scaled by core 0's supervisor. With `trace`, the other core's executor shows
up as its own line in the stats; register `trace::set_core_id_fn` (one line, e.g. read
`SIO.CPUID` on RP2350) to keep `trace-nested` exact per core. Explicit non-goals: task
migration and work stealing (futures aren't `Send` across most HALs; each node lives
where the graph puts it).

### The supervisor on an interrupt tier

The supervisor itself can run on an `InterruptExecutor`, with most graph nodes left in thread mode via `default executor`:

```rust,ignore
supervisor_graph! {
    default executor THREAD;
    executor HIGH;
    node WATCHDOG  = Terminate, task: watchdog_worker;                  // inherits THREAD
    node HEARTBEAT = Pause, executor: HIGH, task: heartbeat_worker;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    THREAD.set(spawner.make_send());
    interrupt::SWI_IRQ_1.set_priority(Priority::P1);
    EXECUTOR_SUP.start(interrupt::SWI_IRQ_1).spawn(app_supervisor().unwrap());
}

#[embassy_executor::task]
async fn app_supervisor() {
    let spawner = unsafe { Spawner::for_current_executor() }.await;
    let sup = Supervisor::new(&GRAPH);
    loop {
        let fault = sup.run(&spawner).await;
        error!("supervisor: {}", fault);
    }
}
```

This keeps fault handling responsive even if a thread-mode task hogs its executor.
See the [demo firmware](https://github.com/cedrivard/embassy-supervisor/blob/main/firmware/README.md#executors).

Five hazards when inverting:

1. Shared state across tiers needs a preemption-safe lock. `local` slots enforce one
   executor per slot, but values reached through accessors do not.
2. `spawn:` arguments and `state:` boxes run at the supervisor's priority. `task:`
   arguments run on the target tier.
3. The supervisor's own work runs elevated. That is the point, and the cost.
4. A hardware watchdog feeder on the elevated tier can mask hangs on the tier it
   watches. Make the feed conditional on the watched tier's health:
   `if GRAPH.all_fresh() { wd.feed(..) }` (`liveness-monitor`) withholds it while any
   node with a `beat_timeout:` is running but not beating.
5. A second core cannot wake an `InterruptExecutor` directly. Embassy pends the IRQ
   in the local NVIC only, so cross-core wakes from remote acks are lost and stall
   the supervisor. Relay the pend through a doorbell, as the demo does.

### Respawning across tiers: resident shells and the executor fence

A task's slot in its `TaskPool` frees only when its **final poll returns**: after its
last statement, inside the executor, where nothing can signal it. A supervisor on a
higher-priority tier (or another core) wakes on the task's ack while that poll is still
running, so a respawn issued right then finds the slot taken. Retrying in a loop is no
answer: on a preempting tier the loop starves the very poll it is waiting for. The
supervisor never sleeps or spins on it instead:

- **`task:` nodes** have resident shells (see
  [`task:`](crate::guide::dsl#task-generated-shells-for-plain-or-generic-workers)): spawned once, they park
  after each activation, and the supervisor starts the next one only once the shell has
  parked, an exact event published as the shell's last act.
- **`spawn:` nodes** are re-spawned through **the executor fence**: `supervisor_graph!` gives
  every `executor NAME;` a small resident task on that tier, spawned when the slot is
  `set()`. A `Busy` respawn asks it for one pass and retries when it reports. An executor
  polls one task at a time, so by the time the fence runs, the old task's poll has
  returned. A node with no `executor:` spawns on the supervisor's own executor, where a
  yield is that pass.

One pass suffices when a `spawn:` task acks as its last act (`mark_exited`/`ack_dropped`,
then return). A body that awaits something after its ack costs one pass per retry until
it returns or its `slot_timeout` faults the respawn with `FaultKind::Spawn`. Each fence costs
one parked task per executor; a hand-built `SpawnerSlot` without `with_fence` has no pass to
offer and retries on the next gate event or at the deadline.
