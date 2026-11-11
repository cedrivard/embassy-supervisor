# Dataflow supervision

Everything above orders *when tasks run*. This part is about the other half of the
graph: *what data flows between them* while they run: declared, turned into a
heartbeat and a readiness assertion, and (at the top tier) derived straight from the
code. It is layered: each feature below builds on the previous one, and stopping at
any layer is fine.

### Spawn ordering is not runtime coupling

A `deps:` edge orders **spawns**, consumed once, at bring-up, never again.
Dataflow (who writes and who reads each signal, for the whole run) is a
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

- Dataflow may be cyclic; a spawn DAG cannot, which is why the coupling tables
  never feed the topological sort.
- A `deps:` edge is never re-evaluated: nothing re-gates a running node when a
  provider cycles underneath it (`epochs` lets it notice; `restart` re-gates it).
- Readiness is a rendezvous, not a persistent guarantee: without `bound-deps`, a
  provider that comes up and later goes quiet is invisible.
- A resource is not a coupling either: it is a value handed over, bound to its
  provider's lifetime. [Resource or signal?](crate::guide::dsl#resource-or-signal) draws the line.

### The pieces, in plain terms

The record dataflow supervision keeps: **for each signal, which node writes it,
which nodes read it.** What each side gets from being on it:

- **Writer**: found by `GRAPH.writers_of(&SIG)`, drawn by the diagram tool; with
  the features below, the write can become the node's heartbeat and its
  readiness ("ready" = actually producing). Readers' gates resolve against it.
- **Reader**: `readers_of(&SIG)` answers "who is affected if this producer
  cycles?"; the diagram tool warns on one-sided signals. Reads carry no
  bookkeeping: the record is the whole product.

Two ways onto the record, differing in who keeps it true:

- **Declared**: `reads:` / `writes:` lists on the node
  ([below](#declaring-the-dataflow-feature-coupling)). Works for any body;
  nothing checks the list against the code.
- **Derived**: `#[embassy_supervisor::dataflow]` on a fn that accesses signals
  through its node: `node.put(&SIG, v)` / `node.get(&SIG)` perform the access,
  `node.writer(&SIG)` / `node.reader(&SIG)` hand the signal back. The attribute
  scans for those calls and emits the fn's tables beside it: the call site *is*
  the declaration, so it cannot drift.

The graph binds derived tables with two keywords:

- **`discover`**: use the `task:` fn's own table as the node's declaration.
- **`dataflow: [path]`**: adopt annotated helper fns. The scan sees one body at
  a time, so helpers must be annotated and adopted by their callers; a module of
  them adopts as one `#[dataflow_bundle]` entry (`dataflow: [crate::api::BUNDLE]`).

Use cases:

| you are a… | use |
| --- | --- |
| writer or reader in your own task fn | the verbs + `#[dataflow]` + `discover` |
| writer through a setter other nodes call | annotate the setter; each caller adopts it; the write attributes to the caller, and the static stays private to its module |
| reader through an accessor | same: annotate it, callers adopt it |
| writer or reader in a body that cannot see the supervisor (third-party, [`cancel`](crate::guide::dsl#cancel-supervisor-unaware-workers)) | `writes:` / `reads:` lists (`observed beat` adds liveness) |
| writer the scan cannot follow (runtime-computed index) | keep that entry in the list |
| reader of a value meaningless until its writer runs | gate it: `deps: [X ready]` (the spawn waits) or a [gated read](#gated-reads-feature-data-deps) `node.open(&SIG)` (each use waits, resolved via the declared writer) |

All of it composes per node: one table per source (list, task fn, each
adoption). An access on no bound table still works; the graph just doesn't see
it.

### Declaring the dataflow (feature `coupling`)

The hand-written tier, for bodies that cannot carry a node. Entries name the
**actual signal statics**: checked to exist and be `Sync`, so a rename breaks
every referring declaration, and that is the whole check; the list is never
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
task (no node in its signature, no `beat()`, no `set_ready()`), which makes this
the tier for a body you do not own.

Today the heartbeat is the only consumer, so an `observed` entry without `beat` is
inert: declared, one word of flash, and never called. The same is true of every
`observed` entry in a `reads:` list, since `beat` on a read is a compile error. They
are not errors: a second consumer (an input that went quiet, a rate readout) would
give them meaning without a syntax change, but nothing asks about them today.

```rust,ignore
observe writes: it.load(Ordering::Relaxed);   // graph-level default; `it` is the signal

node ESTIMATOR = Terminate, beat_timeout: 1000,
    writes: [crate::signals::ESTIMATE observed],
    task: crate::estimator_task;
```

The accessor resolves from three places, most specific first: `observed via <expr>` on
the entry, the graph-level `observe writes:/reads:` default for its direction, and,
with neither, the `Observable` trait from `embassy-supervisor-observe`. That last one
is a leaf facade in the `log` mold: a signal library implements it (or wraps a primitive
in the facade's `Counted`) without ever depending on the supervisor, which is what a
trait defined *here* could never offer. The atomics implement it out of the box, value
as token. The expression forms remain what reach an accessor no trait method could: a
different accessor per entry, or one element of an array (`ARR[1] observed`).

Two things follow, and they are the point of the feature:

- **With `liveness-monitor`, a `beat`-qualified write drives the node's heartbeat.** The
  sweep treats an advancing counter as a beat, so the node above calls `beat()` only on
  its steady path where it produces no output. Name the wrong signal and the node goes
  **stale**: the declaration stops being a comment.

  `beat` is a separate qualifier because the two questions are different: `observed`
  says the signal can be asked, `beat` says the answer is this node's sign of life. A
  node with four outputs usually wants only one treated that way; without the split,
  marking a second write would quietly weaken the heartbeat from "the output that
  matters moved" to "any output moved". `beat` on a `reads:` entry is a compile
  error: a heartbeat is something a node produces.
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

Requires an `observed beat` entry in `writes:` (the sweep's poll of that write is
what asserts the readiness) and `beat_timeout:`, because the sweep only visits nodes
with a budget. Both are compile errors otherwise, because either alone would be a
silent no-op. A body that carries its own heartbeat through the verbs asserts its own
readiness too, with `set_ready()` at the same write. (A `discover` node reaches the
polled form with a marker-only entry beside it, below.)

`Supervisor::run` runs the monitor from its first poll, alongside the boot wave, so the
assertion can gate dependents at boot. A hand-written driver must do the same, selecting
`sup.monitor()` against its `start()` and driver loop: nothing sweeps otherwise. While
the node has yet to assert it is probed at an eighth of its budget, at most 12.5 ms
during its first 100 ms; after that only at an eighth of its budget, so a node that
never writes cannot hold every sweep at 12.5 ms. A body that also calls `beat()`
restarts those 100 ms with each call.

**Monotone: it never withdraws readiness.** A node that later goes quiet is reported
through `wait_health()`, and what that means is the application's decision, exactly as
with `liveness-monitor`. Withdrawing readiness here would leave a node permanently unready
with nothing able to restore it; if you want that coupling, the composition already
exists: `wait_health()` → `Stale` → your own `clear_ready()`.

**The boundary, stated plainly.** Polling can only ever watch a signal the
declaration names; it can never see what a body actually touched, and its resolution
is the sweep interval. That is the price of asking nothing of the task. The tier
below crosses both limits at the price polling exists to avoid: the body must see
the supervisor.

### The node as the access path (feature `dataflow`)

`observed` is the implicit tier: the supervisor asks the signal, and neither the
signal crate nor the task body ever learns it exists. The explicit tier is for a body that holds its
`TaskNode`, the split AUTOSAR ships as `Rte_IWrite` beside `Rte_Write`: the access goes
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
carry its table entry: nothing is registered, nothing is declared, and the record
cannot drift from the code. `put`/`get` perform the operation themselves through the
facade's `Sink`/`Source` traits (the atomics implement both; a signal library adds
one-line impls); the pass-through pair exists for the two patterns no value verb can
express: read-modify-write has no value to hand over without racing
(`node.writer(&COUNT).fetch_add(1, ..)`), and a consuming read needs per-consumer
handle state a shared static cannot carry. A derived table states couplings and
nothing else: the sign of life is carried by the verb, `node.beat_put(&SIG, v)` /
`node.beat_writer(&SIG)`, or by a `node.beat()` call. The scan is receiver-keyed on the
node parameter, so a
`map.get(&key)` elsewhere is never touched; an aliased receiver or a helper fn's
accesses are outside it, so annotate helpers too and adopt their tables; an access
no bound table carries still performs, it simply leaves the graph unaware of it.

`#[dataflow]` goes above `#[embassy_executor::task]`; the other order is a compile
error. A `#[cfg]` in the body gates the entries it covers: one on a statement, a block,
a match arm, a field or an element, and an `if #[cfg(p)] { .. } else { .. }` chain
inside a macro invocation (the `cfg_if!` shape), where each arm is gated on its own
predicate and on the negation of every arm before it. A `#[cfg]` on a closure parameter
is not understood: the scan takes it to gate the closure's body.

### Verbs of your own

The verbs are inherent methods on `TaskNode`, so an extension trait can add more; what
the scan needs is to know the ident and which way it points. The attribute's arguments
say so, and say nothing else:

```rust,ignore
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
the signal back with `s.target`, as `reader` does. Registrations are **additive** (the
built-in verbs are always recognised, and naming one of them is an error rather than a
silent redefinition) and **per fn**, so the same method is an ordinary call in a fn
that does not register it, with its coupling simply absent from that fn's tables.

Direction is stated rather than inferred because the scan is token-level and has no
type information, and direction is not cosmetic: `writers_of`/`readers_of`, the
heartbeat and the gate's producer lookup all partition on it. The diagram tool reads
the same attribute, so a registered verb reaches the diagrams with no configuration of
its own, drawn under its own name.

A crate with a house verb set will want its own attribute wrapping `#[dataflow(..)]`,
since the registration otherwise repeats on every annotated fn; that needs a
proc-macro crate on your side, and is the main ergonomic cost of adopting this.

What the record feeds depends on the node's shape:

- **`discover` (the body is the declaration).** The node binds the tables its `task:` fn's
  `#[dataflow]` attribute derived, in place of `reads:`/`writes:` lists: flash-const,
  and true by construction: the entry exists because the call does, so there is no
  second place to keep in sync. The signal queries and the diagram tool see those
  edges.

  A derived table states couplings and marks none, so a list may sit beside
  `discover` **to add markers only**: every entry must carry `observed`, `beat` and/or
  `veto`, and must name a signal the scan already found. An unmarked entry is a
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
  although it is legitimate, and two signals sharing a final segment pass it,
  in which case the marker simply never matches a write at runtime and the node
  reads as stale. Otherwise the node beats with `node.beat()` and asserts
  readiness with `node.set_ready()`.
- **Declared lists.** Where derivation cannot follow the code (a runtime index, a
  `#[cfg]` on a closure parameter, a table walked at runtime), keep the lists and route the
  accesses through the node anyway, and let `beat_put`/`beat_writer` carry the
  heartbeat at the write that proves it: no marker, no sweep, no clock read.

**Encapsulated signals keep their privacy.** A signal accessed only through a
getter/setter stays fully private: the accessor takes the caller's node, carries
`#[dataflow]`, and callers adopt it:

```rust,ignore
// heartbeat.rs: the static never leaves the module
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

// dataflow: [crate::api::BUNDLE]: exactly the members' tables, concatenated
```

A member fn's own `#[cfg]` gates every entry it contributes, and the graph cannot
tell a bundle from a fn: the emitted statics share the naming.

The tiers compose per node. Keep `observed` where signals and bodies stay foreign to
the supervisor; go through the node where it is already in hand.

**Teardown is a wave.** Every stop path (`teardown`, `teardown_continue`,
`deactivate`, `restart`'s down half, the `bound-deps` cascade) signals a node the
moment every dependent stopping with it has acked, signals nodes with no such
dependents up front, and re-runs the scan on each ack. That is what lets a producer's
shutdown *wait on* an unordered consumer: a `drain()` reached before the consumer has
acked is released by a consumer that has already been told to go, where signalling one
node at a time would deadlock, the producer holding its ack for the consumer, the
supervisor holding the consumer for the producer. And because a `deps:` dependency is
not even signalled until its dependents are gone, it keeps *serving* through their
cleanup: a dependent may flush over a link, or drive one last ioctl through a runner
it depends on, inside its own shutdown.

The unordered side is the contract to write for. A node with no `deps:` path to
another may be told to stop while that other node is still running, and it frees what
it owns as soon as it acks. So a node that publishes a handle to consumers it has *no
edge from* must hold its own shutdown until they let go (that is what `Leased` is),
and what a node uses while shutting down must be one of its `deps:`; an unordered
service cannot be assumed to still work. A node that only acks and exits is
unaffected, which is most of them.

**Bring-up is a wave too.** `start`, `respawn_terminate`, `activate`
and `restart`'s up half spawn every node whose in-pass deps are up and whose gates test
satisfied on each round, parking between rounds on a gate-event signal fired by
`provide`/`restore`, `SpawnerSlot::set` and `set_ready`, so independent slow bring-ups
overlap instead of queueing, and a provider may even be declared after its consumers
with no dep edge (`tests/bringup_concurrent.rs`). Spawn
ordering is strict: a dependent never spawns before its in-pass deps. The
non-blocking gate test this needs is *emptiness*, which means "valid" for every slot
kind, provided a provider whose values die with it clears its `shared` slots on the
way down ([`provides:`](crate::guide::dsl#provides-slots-that-die-with-their-producer), or `ResourceSlot::clear()` before the ack). A node's
`slot_timeout` covers all its gates together, from when its deps resolve.

**The heartbeat only says something when someone asks**, so high-rate writers stay
cheap. A plain `put`/`writer` costs nothing beyond the write itself; a `beat_put`/
`beat_writer` adds one relaxed store. Reads are pure pass-throughs. The
heartbeat is a flag that whoever next asks about staleness (the monitor's sweep, an
app watchdog, a dashboard) converts into a beat inside `ticks_since_beat`, using the
clock read that call makes anyway. A 1 MHz writer therefore pays a relaxed store per
message, never a timer read. The price is resolution: a beat materializes when someone
looks, so the monitor reports a stall between one and two `beat_timeout:`s after the
last beat. (Readiness is the exception, asserted at the access: it is once per
activation and other nodes' bring-up waits on it.) One ordering caveat for hot paths:
`put` writes `Relaxed` through the facade, so a flag that publishes other memory keeps
its own ordering via `node.writer(&FLAG).store(v, Release)`.

The diagram tool reads the same source the graph came from with the same scanner the
attribute uses, so `supervisor-mermaid --runtime` draws the derived edges too: by
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
`producer_of` looks it up by address over the caller's own graph, which covers
`discover`-derived tables no declaration site could name. It is all const: nothing to
register, nothing to size, and a gate resolves before `Supervisor::start` as readily as
after. Two graphs in one binary never answer for each other.

Write your own by implementing `Gated` for your wrapper: `ensure` receives the reading
node and the coupling entry, so it can log the caller, wait on a mode, throttle a first
access: anything, including awaiting. There is deliberately **no blanket impl**:
`open` on an ungated signal is a compile error, not a no-op the reader would mistake
for a guarantee. Starting the producer needs `control` (the mailbox the request goes
through); without it the gate is the readiness wait alone, which is right for a
boot-started producer and reported for any other. Opening a gate sends `Activate` for a
producer that is down, which also clears a `disabled` latch set by an operator or by
`enabled_if:`.

`open` is the only awaiting verb (a gate fires once per consumer at setup, so the
future belongs there and not on every access), and it grants no exclusive access.
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
the one that bites: a task that published a handle into a `static` (a network
stack, a DMA buffer, a peripheral view) cannot free it while a consumer is
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
gets `None` (the honest answer, and one it can act on) rather than a handle
about to dangle, so the producer is not racing an asker. A producer that comes
back up calls `reopen`.

`lease` is sync, unlike `open`: there is nothing to wait for, since either the
value is available or its producer is going away. It records a read like
`reader` does, and the diagram draws it as its own kind of edge.

What this buys over a `deps:` edge is exactness. The count covers **every**
holder, including a consumer whose access no table carries, and a `detached`
node, which teardown skips entirely and which therefore escapes any
ordering-based scheme. The failure mode is a leaked guard: `drain` never
returns and the producer misses its shutdown ack, which surfaces as the
ordinary ack timeout naming the producer rather than as a use-after-free.

Costs one `AtomicU32` and one `Signal` per leased signal, and nothing at all
for signals that do not opt in. `Deref` keeps the wrapped signal's own API
reachable, which also means reaching the static directly is uncounted: the
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
    node TRIP_LOGIC = Terminate, task: trip_logic, discover;
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
observed beat]` makes the gate's flip counter the writer's heartbeat: it moves when the
gate flips either way (the assert that trips it, the release that clears it), by any of
its writers, so a writer whose gate never flips reads stale and one writer's flip beats
for all of them. All writers of one gate must live in the same graph: each
`supervisor_graph!` numbers its own writers from slot 0 (fragments composed into it count
as one graph), so writers in two graphs would share bits, one's release could clear the
other's trip, and neither graph's capacity check would count the other's writers.

### Channels, mutexes, and anything else `Sync`

None of this is about signals. `CouplingPoint` is a blanket impl over every `Sync`
type and identity is the static's address, so a coupling names **any `'static` two
nodes both touch**: a `Channel`, a `Mutex`, a driver handle, whatever the application
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
  which are **sync and infallible**: last-value-wins over a cell. A bounded `send` is
  async and a `try_send` can fail, so neither primitive implements them and
  `node.put(&CHAN, v)` is a compile error rather than a silent block-or-drop. Use the
  pass-through verbs, or register your own: `fn offer(&self, s: Sig<Channel<..>>, v: u32)
  -> bool` over `try_send` is exactly the shape the built-in verbs have no room for.
  Do not implement `Sink` for a queue to make `put` compile.
- **A queue's `len()` is not a change token.** The `observed` sweep compares successive
  readings **for inequality only**, so a channel that fills and drains between two
  sweeps reads identical and looks silent, which under `liveness-monitor` reports the
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
opposite orders is a coupling the graph will draw and never object to: the `deps:` DAG
is checked for cycles, the coupling table deliberately is not. And a `'static` both
nodes can name is always a coupling, never a resource ([Resource or signal?](crate::guide::dsl#resource-or-signal)).

### A status line (feature `node-status`)

`sd_notify`'s third verb, `STATUS=`. A node describes what it is doing in one line
(`node.report_status("receiving image")`), and `node.status()` hands it to whoever asks:
a dashboard, a shell command, a log line on change. Purely descriptive: never an event,
never acted on, cleared when the node (re)starts so a fresh activation does not wear the
previous one's last words.

### The app-owned health monitor

The supervisor reports; the application decides. Every input is a cheap atomic load:

```rust,ignore
for (i, node) in GRAPH.iter_nodes() {
    let down    = !node.is_running();
    let stalled = node.is_stale(Duration::from_secs(1)); // `liveness`
    let unready = !node.is_ready();                      // `readiness`
    let gen     = node.epoch();                          // `epochs`
    // GRAPH.deps_of(i) / GRAPH.dependents_of(i, &mut |d| ..) for the topology
}
```

With `liveness-monitor` the polling is done for you: declare `beat_timeout:` on the
nodes whose bodies beat, and consume `wait_health()`.

**The hardware watchdog as the backstop.** `GRAPH.all_fresh()` (`liveness-monitor`) is
the same policing as one synchronous `bool`: `true` unless a running, non-detached node
with a `beat_timeout:` has gone without a beat for longer than it. Feed the hardware
watchdog only while it holds and a wedged node resets the chip, even if the app's
health consumer is the thing that hung. `GRAPH.stale_nodes()` names the culprits for
the log line. It grants pending beats as it looks (like the monitor's sweep) and has no
`beat_window:` hysteresis, so size the hardware timeout to cover a node's jitter.

**Write freshness is not value validity.** The monitor only answers whether a node
wrote recently. `Stamped<T>` lets a reader check a value's age, but that means
"written within `max_age`", not "this exact read is correct". It cannot detect
semantic problems like a clock servo drifting while still writing. The application
must decide what a stale report means.

**What to do about a report is deliberately yours.** Where a subsystem can be cycled
safely, feeding a `Stale` event to `Supervisor::restart` (feature `restart`) or to
`clear_ready()` across a `bound` edge is reasonable. Where it cannot be
(a flight control loop, a motor commutation task, anything holding physical state),
restarting is the wrong reflex and degrading to a safe mode is the right one. The
supervisor cannot tell those apart, so it does not try, and `liveness-monitor` stays
report-only by design.

### Testing a coupling claim

Whether a coupling actually carries data is an app-side test pattern, because only the
app can build a valid sample: with the mock clock, bring the graph up, inject into a
producer's signal, and assert the consumer's `beat()` advances (`ticks_since_beat`) or
its own output changes, within a bounded mock interval.
