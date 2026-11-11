# The `supervisor_graph!` DSL

The complete grammar is summarized in [The grammar, at a glance](crate#the-grammar-at-a-glance);
this chapter walks through each clause.

### Spawn forms

A bare path `f` spawns `f(&NAME)`; a partial call `f(a, b)` spawns `f(&NAME, a, b)` (the node
is always injected first, except under [`cancel`](#cancel-supervisor-unaware-workers),
which suppresses it); a closure is emitted verbatim (nodes only). These forms apply to
both `spawn:` (a hand-written `#[embassy_executor::task]` fn) and `task:` (a plain worker fn
the macro wraps). **Prefer `task:`**; see
[`spawn:` vs `task:`](#spawn-vs-task-which-to-use) for the cases where `spawn:` is the
right tool. Omit both for a **parked** node whose task the application spawns itself (e.g. a
`Pause` sensor holding a peripheral handle); the supervisor tracks it but never spawns it.

### `task:` (generated shells for plain or generic workers)

`spawn:` names a hand-written `#[embassy_executor::task]` fn. `task:` instead names a **plain
async fn** (possibly generic) and the macro stamps the concrete
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

- Same path / partial-call forms as `spawn:` (no closures: the shell needs a name to call).
- **Worker args are evaluated inside the shell**, at the start of each activation, on the
  node's own executor, so the DSL never needs the arg types, an `executor:`/second-core node
  builds its resources on the core that runs them, and cross-node data should go through
  awaited accessors (a spawn batch polls last-first). Corollary: an extra that can be
  **missing** then is a task-side panic, not a failed spawn: extras are for infallible
  accessors. A value that might not exist yet belongs in `resources:` (a `shared` entry for a
  fan-out handle): the pre-spawn gate turns "missing" into a clean `FaultKind::ResourceMissing`.
- **The shell is resident.** It is spawned once, by the node's first activation, and never
  returns: each activation runs the worker, puts back what it lent, records the exit, and
  parks; the next start (restart, respawn, `activate`, pool growth) wakes it in place instead
  of spawning again. A restart therefore never waits for a task slot to free, which embassy
  does only after the task's final poll returns, out of the supervisor's sight, and which a
  supervisor on a higher-priority tier would otherwise race. The supervisor starts the next
  activation only once the shell has parked, so nothing the previous one does late can land
  on the new one. The worker future is still rebuilt per activation; the task slot is
  static either way, so residency costs no RAM.
- On a `pool`, `task:` emits ONE shell sized to the member count; each member keeps its own
  resident instance.
- `state:` crosses from the spawn glue into the resident shell through a handoff slot, so the
  state type must be `Send`.
- Trace adoption and `executor:` routing compose exactly as with `spawn:`.
- The ceiling embassy imposes still stands: concrete types are fixed per binary; `task:`
  removes the boilerplate, not the monomorphization.

### `spawn:` vs `task:` (which to use)

**Prefer `task:`.** It drops the `#[embassy_executor::task]` boilerplate, admits generic
workers, sizes a pool's `TaskPool` from the member count automatically (no
`pool_size = MAX` constant to keep in sync with the DSL's `max:`), and is the only form
that supports `resources:`. The generated shell is free at runtime: its wrapper inlines
into the same poll, and its `TaskPool` static simply replaces the one the attribute would
have emitted.

`spawn:` remains the right tool in four situations:

1. **The task fn already carries `#[embassy_executor::task]` and you can't (or shouldn't)
   strip it**: it lives in another crate, or other code depends on it staying a task fn.
   `task:` needs a *plain* async fn to wrap; a token-returning task fn can't be re-wrapped.

   ```rust,ignore
   // other_crate exports: #[embassy_executor::task] pub async fn modem_task(node: &'static TaskNode) { .. }
   node MODEM = Terminate, spawn: other_crate::modem_task;   // spawns modem_task(&MODEM)
   ```

2. **The same task is also spawned outside the graph.** `spawn:` reuses the one existing
   `TaskPool`; `task:` would stamp a second shell + pool, duplicate RAM for the same
   future type.

   ```rust,ignore
   #[embassy_executor::task(pool_size = 2)]
   async fn logger(node: &'static TaskNode, sink: Sink) { /* ... */ }

   // One instance supervised ...
   node LOG = Pause, spawn: logger(uart_sink());
   // ... and one the app spawns by hand into logger's pool, as a parked node:
   node USB_LOG = Pause;
   spawner.spawn(logger(&USB_LOG, usb_sink()).unwrap());
   ```

3. **Custom spawn-time logic**: the verbatim closure form (nodes only). `task:` rejects
   closures (the shell needs a name to call).

   ```rust,ignore
   node SENSOR = Terminate, deps: [BUS],
       spawn: |s: Spawner| {
           let token = sensor_task(&SENSOR, if fast_variant() { Odr::Hz30 } else { Odr::Hz8 })?;
           SENSOR.adopt(&token);   // closures bypass the macro's trace glue; adopt by hand
           s.spawn(token);
           Ok(())
       };
   ```

   ⚠️ The `adopt` line is **your job, and nothing will remind you**: the closure owns the
   `SpawnToken`, so the macro cannot capture the task id (`trace`) or stamp the node name
   (`metadata-names`) for you, and a stable proc-macro cannot emit a warning. Forgetting it
   is silent: the node simply never appears in the trace/name output.

4. **Arguments that must be evaluated at spawn time, on the supervisor's executor.**
   `spawn:` partial-call args run in the spawn glue, at the moment of the (re)spawn;
   `task:` extras run inside the shell at the *start of each activation, on the node's
   own executor*.
   The `task:` behavior is what you usually want (an `executor:`/second-core node builds
   its state on the core that runs it); reach for `spawn:` when an argument snapshots
   something that must be read *now* or must not run on the target tier.

   ```rust,ignore
   // Snapshot the respawn count at the moment of this spawn, not when the activation
   // starts (an interrupt-tier node's start can preempt and land arbitrarily later):
   node REPORT = Terminate, executor: HIGH, spawn: report_task(boot_epoch());
   ```

Omitting both keeps the node **parked** (see [Spawn forms](#spawn-forms)); that's a third
option, not a tie-breaker between the two.

### `resources:` (owned values handed over at spawn)

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
LED.provide(Output::new(p.PIN_25, Level::Low)); // consumes p.PIN_25: no steal, no 2nd owner
sup.start(&spawner).await?;
```

- **a provider node**, for anything another task builds at runtime (a radio stack
  after its firmware load, a serial stream once its UART is pumping). It names the
  slots it fills with [`provides:`](#provides-slots-that-die-with-their-producer) and the consumers order after it with
  `deps:`; the [provider-node recipe](crate::guide::recipes#provider-node-async-multi-output-construction-in-the-graph)
  shows the whole shape.

The protocol, per (re)spawn:

1. The value is `provide()`d once. Consuming the `Peripherals` field is the
   **compile-time exclusive-ownership guarantee**: a second owner cannot exist.
2. The generated glue probes the slot just before the spawn. An unprovided slot fails
   `Supervisor::start` with `FaultKind::ResourceMissing` after a bounded wait (the
   node's `slot_timeout`, 100 ms by default; the supervisor logs the node name):
   fail-closed at bring-up, not a panic inside a running task. Provisioning is the
   runtime-checked half of the contract.
3. The generated shell `take()`s the value at the start of each activation (never
   through the task-fn call, where a `Busy` storage claim would drop it, unrecoverable),
   hands the worker
   `&mut Type` (after the node arg, in declared order, before any partial-call extras) and
   `restore()`s it after the worker returns (i.e. after its shutdown ack). A Terminate
   respawn therefore re-takes the **same instance**; a Pause worker never returns, so it
   simply retains its resources.

The supervisor awaits a node's slots being filled before each (re)spawn (same bounded wait
as `executor` slots), so late provisioning and the respawn-vs-restore window on another core
are both covered. Caveat: a panic in the worker skips the restore (embedded panic = reboot).

`ResourceSlot<T>`'s hand-usable API, for providers and app code (the macro's glue uses
the same calls): `provide(T)` / `restore(T)` fill the slot (restore is provide, named for
the give-it-back half), `take() -> Option<T>` empties it, `get() -> Option<T>`
(`T: Copy` only) copies without emptying, `clear()` empties it and resets the filled
latch, and `async wait_take() -> T` awaits a fill then takes, which is how an `exit:` slot is
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
| *(default)* | `&mut Type` | `restore()`d; respawn re-takes the same instance | long-lived singletons (`Output`, a reborrowable `Peri`) |
| `consume` | `Type` **by value** (shell `take()`s) | nothing: the slot stays **empty** | resources the worker must *drop* at teardown (a driver whose `Drop` releases pins/DMA) or that go stale across a power cycle and are rebuilt each run |
| `shared` | `Type` **by value** (shell **copies** via `get()`, `T: Copy`) | nothing: the slot **stays filled** | one handle fanned out to many consumers (`embassy_net::Stack`, a `&'static` shared-bus ref); several nodes (and whole `task:` pools) declare the SAME slot name |
| `local` | as the kind it composes with | as the kind it composes with | `!Send` values (`RefCell`-/`NoopRawMutex`-based driver handles) that never leave the **one executor** every declaration of the slot runs on |
| `divisible` | a `Claimant` **by value**: its own slot in the graph's `Budget<K>` | its share is **released** by the supervisor (`Pause` parks keep it) | one quantity split among N holders, where a dead holder must not strand its share |

`consume` makes teardown-drop explicit and turns the wake path into "build fresh, `provide()`,
respawn": until the application re-provides, a respawn fail-closes with `FaultKind::ResourceMissing`
instead of reusing a stale instance.

`shared` replaces the panicking-accessor pattern for fan-out handles: instead of a
`task:` extra like `stack()` that panics at the start of an activation when the value is missing, a
`shared` resource is gate-awaited before the spawn and a missing value is a clean
`FaultKind::ResourceMissing`. The slot static is emitted once per unique name (with the union of
the declaring sites' `#[cfg]` predicates); every re-declaration must repeat the same
kind markers and type. Entries may also carry per-entry `#[cfg(...)]`; gate the worker
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
carries an `unsafe impl Sync`, the one graph form that injects unsafe
code, hence the explicit opt-in (same reason the `trace-hooks` symbols live at the graph
site). Its soundness contract is: **every touch of a slot's value happens on one
executor.** The macro checks the half it can see: every declaration of the slot, each
consumer's `resources:` entry and the provider's `provides:` entry must resolve to the
same executor, the graph's `default executor` included (any executor qualifies, a second
core too; a mismatch is a compile error naming both declarers and their tiers), and the
provider must be a node the graph spawns (`task:` or a path-form `spawn:`), since a
parked provider runs wherever the application put it. The provider fills and clears the
slot from its own task, the consumer's shell reads and restores it on its executor, and
the supervisor's bring-up probe is value-free, so the supervisor's own tier is never a
party. The half the graph cannot see is yours, which is why the slot's `provide`, `take`,
`restore`, `get` and `clear` are `unsafe fn`s (`is_filled` is safe): a call from `main`
or from a provider's body must run on that same executor. Without the feature a `local`
marker is a compile error naming it, and a consumer crate that forbids `unsafe_code`
cannot use `local`.

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
let runner = build_radio_runner().await;
// SAFETY: called on the slot's executor, the one RADIO runs on.
unsafe { RUNNER.provide(runner) };
```

#### `provides:` (slots that die with their producer)

`provides: [RES, ..]` on a node names the slots its task fills at runtime, resolved
against the graph's `resources:` entries (an unknown name is a compile error). The
node's shutdown ack clears them (the value drops, and a `ResourceSlot`'s filled latch
resets; `Pause` parks excepted, since a parked task still backs what it published), so a
consumer's gate waits for the next activation's value instead of taking a leftover.

**The `shared` slot is why the clause exists.** A `consume` slot is empty from teardown
until the rebuild, so its gate is fresh by construction, but a `shared` slot is never
emptied by its consumers, and "filled" cannot say *whose activation* filled it: after
the provider stops, a gate wait would hand out the previous cycle's handle. Resources
are not couplings, so nothing else links a slot to the task that fills it; the clause
is that link. Emptiness is then the freshness signal the gate waits already
understand. The declared form also covers providers whose ack happens inside
`run_cancellable_acked` or a `cancel` shell, and autonomous exits through
`mark_exited`; the manual form (`ResourceSlot::clear()` before the ack) needs no
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
a consumer must not read it before its writer is up: that is readiness (`deps: [X
ready]`, `ready_on_write`, or a [gated read](crate::guide::dataflow#gated-reads-feature-data-deps)), and the
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
  Where the value *is* the state (the provider has nothing to say beyond "here it is"),
  the slot gate alone is the rendezvous, and a `ready` marker beside it states the
  same fact twice.

### `exit:` (typed exit values)

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
[`cancel`](#cancel-supervisor-unaware-workers)) pointing **at the `exit:` clause**;
drop the clause, or give the worker a return type it can actually reach. A diverging
worker *without* `exit:` stays perfectly legal: that is the shape `cancel`, `Pause`,
and detached daemons exist for.

### `cancel` (supervisor-unaware workers)

Every worker so far takes the node as its first argument and answers the shutdown
handshake itself (`run_cancellable*`, or just returning). `cancel` moves that whole
job into the generated shell: the shell drives the worker under
`run_cancellable` and does **not** pass it the node, so the worker is a plain
`async fn`, the shape an existing firmware already has:

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
usual tail: state freed, resources `restore()`d, exit recorded, so a `Terminate`
respawn re-takes the same instances, exactly as if the worker had been node-aware.
Resources still arrive, now as the *first* arguments (nothing leads them).

With `exit:`, the value is provided only on a **real completion**; an aborted
worker leaves `<NODE>_EXIT` empty (with `shutdown_requested()` set), which is how
a waiter tells "finished" from "stopped". Combining `exit:` with a worker that can
*never* complete is [a compile error](#exit-typed-exit-values). Drop-in-place is
also the trade: the
worker gets no post-cancel code, so a task that must flush or release something
*ordered* at teardown should keep the node argument and race
`run_cancellable_acked` itself.

A `pool` takes the same flag, in any position like every clause. It applies to the
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
worker: a `cancel` member holds no node, so `mark_busy()`/`mark_idle()` are called
on `WORKERS[i]` by whatever hands it work (the member statics are app-visible), not
by the worker itself.

`task:`-only (a `spawn:` fn owns its body and can call `run_cancellable` itself),
and rejected on `Mode::Pause` because a Pause worker must survive the stop and park on
`wait_resume()`, which an exit record nothing ever resumes contradicts.

### `disabled`

Declared but not started at boot; a control `Activate` starts it later (e.g. an OTA task).

### `enabled_if:` (settings-gated bring-up)

*(feature `enabled-if`)*

```rust,ignore
fn lora_on() -> bool { SETTINGS.lock(|s| s.lora) }

supervisor_graph! {
    node LORA = Terminate, task: lora_task, enabled_if: crate::lora_on;
}
```

`disabled` decided at runtime. `Supervisor::start` calls the predicate (any expression
coercing to `fn() -> bool`) once per call, before the wave, for each node that is down;
`false` latches `disabled` exactly as the `disabled` marker would: dashboards see it,
and `activate` / `start_node` still override it. `true` never clears a latch, so a
manual `deactivate` survives a later `start()`. Running and detached nodes are not
evaluated, and neither `respawn_terminate` nor the control verbs re-evaluate: when a
setting changes at runtime, waking the node is the app's call (`activate`).

### `restart_budget:` (restart intensity)

*(feature `restart-intensity`, implies `restart`)*

```rust,ignore
node NET = Terminate, task: net_task, restart_budget: 3 in 60000;
```

OTP's restart intensity, per node: at most `MAX` explicit `Supervisor::restart`s of this
node (directly or through `ControlOp::Restart`) per `WINDOW_MS` window. The `MAX+1`th
inside the window returns `FaultKind::RestartStorm` **before anything is stopped**: the
node keeps running as it was, and `apply_control` propagates it rather than absorbing
it like a startup fault. Only the verb's target counts: dependents cycled with it, and
`activate`, do not. The window opens at the first counted restart; one after it closes
opens a fresh window, however long ago the last one opened. `MAX` must be in 1..=65535
and `WINDOW_MS` at least 1.

### `executor NAME;` and `executor: NAME`

`executor NAME;` declares a `SpawnerSlot` that the app fills with a `SendSpawner`.
Nodes marked `executor: NAME` spawn through that slot. `start()` waits for the slot up
to a bound; an empty slot fails the spawn with `FaultKind::ExecutorSlotEmpty`.
`executor:` requires a `task:` or `spawn:` source and cannot be used with a closure.

The `Send` bound applies to spawn arguments, not the future. A `task:` shell's
arguments are always `Send`, so any `task:` worker can route. A `spawn:` fn's own
arguments must be `Send`. Use `local` slots for tier-local values.

`default executor NAME;` sets a default executor for `task:` and `spawn:` nodes and
pools. Parked nodes and `spawn:` closures stay on the supervisor's executor. An
explicit `executor:` wins. One per graph, not `#[cfg]`-gable. Use it to keep most of
the graph in thread mode while the supervisor runs on an interrupt tier.

### Dependencies

`deps:` names declared nodes *or pools*. A pool name resolves to the pool's **floor member**
(member 0, the `min`-kept one), so `deps: [POOL]` means "start after the pool is up".

**A plain dep orders *spawns*, not *readiness*.** `start()` spawns a node and immediately
marks it running, so a dependent with no gates can race its provider's body. Bring-up
is a wave: a node spawns once its deps are up and its gates (executor slot, resource
slots, ready deps) are satisfied, bounded by its own `slot_timeout`; unrelated nodes do
not wait on each other's gates. Two rendezvous exist, both opt-in, and they carry
different things across the edge:

- a `resources:` slot wait: the provider hands over a **value**; the gate is the
  value's presence ([`provides:`](#provides-slots-that-die-with-their-producer));
- a **`ready` dep marker** *(feature `readiness`)*: the provider asserts a **state**
  with nothing to hand over. `deps: [NET ready]` additionally
  awaits the dep's task-asserted `set_ready()` before spawning this node, bounded by
  this node's `slot_timeout` (then `FaultKind::ReadyDepTimeout { dep }`, which names
  the dep that never asserted). Elastic-pool growth also defers while a ready-marked dep is
  un-ready (a sync check per evaluation, no wait). `ready` on a pool name means the
  floor member's readiness; markers on a `pool`'s own `deps:` apply to every member.

The provider side is three calls: `set_ready()` once serving, `clear_ready()` on a
lost link (**status, not control**: dependents are not stopped; pair with a control
`Deactivate` for a cascade, or opt an individual edge into `bound` below), and the
pre-spawn reset clears it so a respawned provider re-asserts. `wait_ready()` is for app
code, one pre-fill waiter at a time like the other latching gates; fan N waiters out
through an app-owned `embassy_sync::watch::Watch` instead.

Pick by what crosses the edge: a value wants a slot, a state wants `ready`. Both on one
edge for one fact is redundant ([Resource or signal?](#resource-or-signal)).

What a dep edge does *not* say (the continuous flow of data between running
tasks) has its own part: [Dataflow supervision](crate::guide::dataflow).

### `#[cfg(...)]`

Allowed on any `node`/`pool` *and on individual deps*. Absent nodes keep their slot as `None`
and are skipped everywhere at runtime.

A construct that needs a Cargo feature (a `ready` or `bound` dep, `reads:`/`writes:`, an
`observed` or `veto` entry, `discover`, a `dataflow:` entry, `local`, `divisible`,
`state:`, and the `beat_timeout:`, `beat_window:`, `enabled_if:`, `restart_budget:` and
`ready_on_write` clauses) is rejected at expansion only when no `#[cfg]` gates it. Under
a `#[cfg]` on its item, its entry or its clause, the macro leaves it out and emits the
rejection as a `compile_error!` under the same cfgs, so a build that compiles the
construct out does not need the feature.

### `pool`

The mode list declares the members (floor first: typically `[Terminate, OnDemand, ...]`). The
macro generates the member array `NAME: [TaskNode; K]`, the spawn glue (one body for the
whole pool, entered through a tail-calling trampoline per member), a
`NAME_POOL: ElasticPool<P>`, and the structural constants `NAME_MIN` / `NAME_MAX` /
`NAME_MEMBERS` (`usize`).

**Per-member resources.** Take-kind `resources:` entries (the default lend, and
`consume`) become **per-member slot arrays** (`pub static RES: [ResourceSlot<T>; K]`):
member `I` takes and restores element `I` exclusively, so members never contend, the
floor comes up with only floor-many elements provided, and a lend value survives a
shrink/regrow **on the same index** (the per-connection-worker shape). `shared`
entries (`shared local` included) stay one fan-out slot for the whole pool; only
take-kind `local` is rejected on pools (the one-executor slot contract + per-member
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
`const`; deriving the member count from a const is structurally out of reach. Pool clauses
may appear in any order, like every clause; `policy:`, `min:` and `max:` are required. `policy:` takes the scaling policy; annotate
the type explicitly (`policy: DeferredShrink = make_policy()`) when the value isn't a
`Type::new(..)` constructor.

The constants exist for downstream **const-context sizing**: deriving a related capacity
from the DSL instead of duplicating the number by hand (a `const` cannot read the member
`static` array, so `NAME.len()` doesn't work there):

```rust,ignore
// One TCP socket per concurrently-running worker, plus one for DNS:
pub const SOCKET_BUDGET: usize = HTTP_MAX + 1;
let resources = StackResources::<SOCKET_BUDGET>::new();
```

### Limits and compile-time validation

At most **256 slots** per graph: all graph indices are `u8`, which keeps the dep table and
order arrays byte-sized on flash-constrained targets.

The macro rejects an invalid graph at compile time, each with a spanned error at the
offending token:

- **unknown dependency**: a `deps:` name that is not a declared node or pool
- **duplicate dependency**: `deps: [A, A]`, a name repeated in one list under the same
  `#[cfg]`s (one dep under two different `#[cfg]`s is two alternatives)
- **duplicate node/pool name**: a redeclared name would silently rewire earlier deps
- **two items with one runtime name**: names are lower-cased and a pool member goes by
  `<pool><index>`, so `Net` beside `NET`, or node `W0` beside pool `W`, would make a
  lookup by name find only the first
- **unknown `executor:` name**: on a node or pool, checked against declared
  `executor NAME;` slots
- **`executor:` with a closure spawn**: the closure owns the spawn, so routing through a
  slot must happen inside it; only the task-fn-path forms combine with `executor:`
- **malformed spawn form**: anything other than a task-fn path, a partial call, or a
  closure
- **`task:` and `spawn:` together**: mutually exclusive per node/pool
- **a closure in `task:`**: the generated shell needs a worker fn it can name
- **`pool_size:` on a node**: removed, since the resident `task:` shell never needs a second
  task slot; a hand-written `spawn:` task fn declares its own `pool_size`
- **`resources:` without `task:`**: resources are taken/restored by the generated shell; a
  hand-written `spawn:` fn manages its own arguments
- **empty `resources:` list / duplicate resource name**: slot names are statics, unique
  across the whole graph (only `shared` and `divisible` entries may repeat a name,
  verbatim); a name has one kind, so a take-kind entry and a `shared`/`divisible` one
  cannot share it
- **pool `resources:` without `task:`**: the generated shell receives the values (and
  restores lend entries); a `spawn:` task fn manages its own arguments
- **take-kind `local` on a `pool`**: the one-executor slot contract + per-member restore
  is deferred; `shared local` is fine (one pool-wide fan-out slot)
- **a repeated kind marker on a `resources:` entry** (`consume consume T`): declaration bug
- **`local` without the `local-resources` feature**: the kind emits an `unsafe impl Sync`,
  so it is strictly opt-in
- **`shared` with `consume`**: one value cannot both fan out and move by value.
- **`divisible` with any other kind marker, or with a type**: a budget is its own kind.
  Also rejected without the `budget` feature, or with more than 256 slots.
- **`serialized` without `shared`**: serialization applies only to fan-out slots. Also
  rejected when holders span multiple executors.
- **`veto` on a `reads:` entry**: vetoes are asserted, not read. Also rejected without
  the `veto` feature, with more than 32 writers, or when one gate is spelled two ways.
  A `veto` target must be a `VetoGate` with enough slots.
- **a `shared` slot re-declared with different kinds/type**: all declarations of the same
  name must match exactly.
- **a `local` slot declared on two executors**: every declaration must resolve to one
  executor, default included; the value never leaves the tier that built it
- **a `local` slot provided by a node the graph does not spawn** (a parked node, or a
  `spawn:` closure): the application picks its executor, so the slot's tier cannot be
  checked
- **a second `default executor`, one in a fragment, or one under `#[cfg]`**
- **a fragment body that sets `name:` or an `observe` default**: both are graph-wide,
  and the compose site pastes every fragment into one graph
- **`slot_timeout: 0` / `ack_timeout: 0`**: would reject every spawn or stop
- **a suffixed integer literal** (`slot_timeout: 5s`, `restart_budget: 3u8 in 1000`): the
  timeouts, `beat_window:` and `restart_budget:` take plain integers, since the diagram
  tool reads only the digits and the build and the diagram must not disagree
- **an item named like a derived one**: `X_EXIT` beside node `X` with `exit:`, or `P_MAX`
  beside pool `P`; both would be items of the declaring module
- **[`cancel`](#cancel-supervisor-unaware-workers) without `task:`**: the flag only
  rewrites the generated shell; a hand-written `spawn:` fn can race shutdown itself
- **`cancel` with `Pause`**: a Pause worker must survive the stop and park on
  `wait_resume()`, but `cancel` drops its future and
  records an exit; use `Terminate`/`OnDemand`, or drive the pause by hand
- **[`exit:`](#exit-typed-exit-values) on a worker that can never return**: the shell's
  `provide` would be dead code, so nothing could ever fill the slot and every waiter
  would hang; rustc reports it as `unreachable statement` (or `unreachable call` under
  `cancel`) spanned on the `exit:` clause rather than as a macro error. A diverging
  worker *without* `exit:` stays legal
- **pool bounds**: `min <= max <= K` (member count), values must fit `u8`
- **an empty pool, or a `#[cfg]` on one pool member**: gate the whole pool instead
- **`ready_on_write` without `beat_timeout:`, or without an `observed beat` write gated
  like it or not at all**: the monitor sweep asserts the readiness by polling that write
- **pool without the `pool` feature**: a `pool` item requires enabling it
- **more than 256 slots**: the `u8` index cap above
- **dependency cycle**: caught by the `const` topological sort, so it surfaces at
  const-eval of `GRAPH` rather than at macro expansion; still a compile error

Generated surface at the call site: one `pub static` per node, the pool array + `NAME_POOL`
\+ the `NAME_MIN`/`NAME_MAX`/`NAME_MEMBERS` consts,
one `SpawnerSlot` static per `executor NAME;`, one slot static per `resources:` entry (plus,
iff any entry is `local`, the local slot type and its gate view; one `Budget<K>` per
`divisible` name), one
`NAME_EXIT` slot static per `exit:` node, `pub static GRAPH`, and its
`pub type GRAPH_TOPOLOGY` alias (`X_TOPOLOGY` for a graph named `X`). Under `trace-hooks`, an
unnamed graph also defines the seven `_embassy_trace_*` hook fns the executor calls. Every other
item the expansion defines is a private helper named `__SV_*` or `__sv_*`.
