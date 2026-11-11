# Writing supervised tasks (the TaskNode API)

A supervised task is an async fn whose first parameter is its node; the macro's glue
passes it automatically; extra arguments come from the partial-call form
(`task: my_task(EXTRA)`). (A [`cancel`](crate::guide::dsl#cancel-supervisor-unaware-workers) node is
the exception: its worker never sees the node; the shell holds it and uses this same
API on the worker's behalf.) The preferred style is a **plain worker fn** declared with
[`task:`](crate::guide::dsl#task-generated-shells-for-plain-or-generic-workers), and the graph stamps the
`#[embassy_executor::task]` shell for you:

```rust,ignore
async fn my_task(node: &'static TaskNode) { /* ... */ }
```

Alternatively, write the attribute yourself and declare the fn with `spawn:`, needed in a
few situations ([which to use](crate::guide::dsl#spawn-vs-task-which-to-use)). Everything below applies
identically to both styles; only who writes the `#[embassy_executor::task]` differs.

**The canonical loop**: race work against shutdown, return on `Err(Aborted)`:

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

The combinators own the select and the ack. Four rules cover the rest:

1. **Race work against shutdown at every blocking await**: `run_cancellable_acked` and
   `run_pausable_loop` own this; use bare `run_cancellable` only when cleanup must run
   between the cancellation and the ack (flush, unpublish, busy/idle bracketing).
2. **Autonomous exit calls `mark_exited()`**: it acks like `ack_dropped()` *and* records
   the completion, so a worker that returns on its own reads as down and a control
   `Activate` can respawn it. `task:` shells do this automatically; `spawn:` tasks call
   it themselves.
3. **Resources follow the mode**: a `Terminate` task re-acquires everything on respawn
   (drop-on-exit is the cleanup); a `Pause` task keeps what it holds across pause→resume
   and never re-acquires.
4. **A pooled `spawn:` fn acks as its last use of the node**: a `spawn:` task fn with
   no free pool slot (`pool_size = 1`, the default) cannot be respawned until the
   previous instance has returned. With a pooled task fn the respawn can succeed while
   the old instance is still running, so its ack (`ack_dropped`, `mark_exited`, the
   `_acked` combinators) must be the last thing it does to the node: anything after it
   lands on the new activation. `task:` nodes and `task:` pools carry no such rule.

**`Pause` node**: ack, then park; held resources survive. `run_pausable_loop` owns the
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

**Pool worker**: same as `Terminate`, plus load reporting around the busy section:

```rust,ignore
node.mark_busy();                                // idle→busy fires the scale signal
serve_connection(&mut socket).await;
node.mark_idle();                                // busy→idle fires it again
```

Keep `mark_busy()` held for the whole session the worker's resource is tied up. The
connection-bound worker that must stay busy *across* a possible cancellation composes
the pieces like this: busy for the whole serve, the return (whose shell ack completes
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

**Detached daemon / run-once**: detach as the first act, then own your lifecycle:

```rust,ignore
#[embassy_executor::task]
async fn confirm_task(node: &'static TaskNode) {
    node.set_detached(true);                     // supervisor is hands-off from here
    wait_until_ready().await;
    confirm();                                   // runs once and simply returns
}
```

**Parked node** (declared with no `spawn:`): the app spawns it by hand, typically
because it needs values only `main` owns; `adopt` keeps trace attribution working:

```rust,ignore
let token = pump_task(&PUMP, hw_handle).unwrap(); // task fns return Result<SpawnToken, _>
PUMP.adopt(&token);                               // register its task id for trace
spawner.spawn(token);                             // Spawner::spawn takes the token
```

### Method reference

The cancellable combinators return `Result<F::Output, Aborted>` and the pausable one
`Result<F::Output, Resumed>`; both markers are crate types, so
`use embassy_supervisor::{Aborted, Resumed, TaskNode};` covers the canonical loops.

**Lifecycle methods**: the task-side half of the protocol:

| method | role |
|---|---|
| `run_cancellable_acked(fut).await` | the everyday body: race `fut` against shutdown AND complete the handshake on `Err(Aborted)`; discarding the result (`let _ =`) is fine, the ack already happened |
| `run_cancellable(fut).await` | same race, no ack: for cleanup between the cancellation and the ack |
| `run_pausable_loop(body).await` | the whole `Pause` protocol in one call: rebuilds `body` every cycle, never returns |
| `run_pausable(fut).await` | `Pause` bodies: same race, and on a pause it acks, parks, and returns `Err(Resumed)` only after the resume |
| `wait_shutdown().await` | the underlying primitive: park until a stop/pause is requested |
| `ack_dropped()` | complete the handshake: clears `running`, wakes the supervisor's ack wait; `task:` shells call this on return; `run_cancellable_acked` calls it on `Err(Aborted)`; hand-written `spawn:` tasks call it themselves |
| `mark_exited()` | `ack_dropped()` + record the completion (`has_exited()`); call on an autonomous exit; `task:` shells emit it automatically |
| `wait_resume().await` | `Pause` only: park (after acking) until resumed |

**Pool and status methods**: load reporting and observability:

| method | role |
|---|---|
| `mark_busy()` / `mark_idle()` | pool workers: report load; a *real* transition fires the scale signal itself |
| `shutdown_requested()` | synchronous check, e.g. at the loop top before starting new work |
| `has_exited()` | true once the last instance's body returned; cleared by the pre-spawn reset |
| `set_detached(true)` | opt out of supervision from now on (self-managed daemon or run-once) |
| `adopt(&token)` | parked nodes: register a hand-spawned task's id so trace accounting sees it |

**Status reads**: readable from anywhere (a status endpoint iterates `GRAPH.nodes`
and reads these; all are cheap atomic loads):

| method | true when |
|---|---|
| `is_running()` | the supervisor has an instance up (spawned, not acked, not exited) |
| `is_busy()` | the instance reported `mark_busy()` |
| `is_disabled()` | stopped-at-boot or control-`Deactivate`d, and not yet re-activated |
| `is_detached()` | self-managed; every lifecycle op skips it |
| `shutdown_requested()` | a stop/pause was requested; readable until the next pre-spawn reset |
| `is_ready()` *(feature `readiness`)* | the task asserted `set_ready()` |
| `is_stale(max_age)` *(feature `liveness`)* | running but no `beat()` within `max_age` |

Useful compositions: **down** = `!is_running()`; **parked `Pause`** = mode `Pause` +
`!is_running()` + `shutdown_requested()`; **autonomous completion** = `has_exited() &&
!shutdown_requested()`. A spawn that fail-closed (a `NodeFault` from a gate) leaves the
node `!is_running()`: nothing was taken or spawned. Ordering guarantees: when
`stop_node`/`teardown`/`deactivate` return `Ok`, the ack has happened and
`is_running()` is already `false`. For bodies that ack by returning (the
`run_cancellable_acked` idiom), `has_exited()` is also already `true` when the
supervisor cannot run inside the node's poll (same executor, or a lower-priority one on
the same core). A supervisor on a higher-priority executor or another core can see the
ack before the shell records the return, so read the outcome from the node's `exit:`
slot (`<NODE>_EXIT.wait_take()`) instead.

### Special case: workers that never return

A worker typed `-> !` (or a service contract returning a `Never` type) opts out of
`Terminate`/restart *by type*: the body can never return, so stop and respawn
semantics are inert on it as-is. Two ways in: add
[`cancel`](crate::guide::dsl#cancel-supervisor-unaware-workers) to the node and the generated shell
races the body against shutdown for you (no signature change, the future is dropped
in place), or keep the node argument and race the work yourself (one
`run_cancellable` call) when teardown needs ordered post-cancel work.
`Pause`-parked and detached daemons are the forms that legitimately never return.
