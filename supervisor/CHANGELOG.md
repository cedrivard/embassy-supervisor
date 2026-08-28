# Changelog

All notable changes to `embassy-supervisor` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0] - 2026-08-27

Macro pin moves to `embassy-supervisor-macros = "=0.8.0"`.

A graph that forwards this crate's optional features from its own cargo features
can now say so in the declaration: every value-level clause takes a `#[cfg(...)]`
gate, so one graph source compiles with and without the feature. The other theme
is hosted builds: the `defmt` backend is embedded-only now, which makes every
feature combination — `--all-features` included — testable on the host, and a
one-call stderr sink covers the simulator-logging gap that opens.

### Added

- `#[cfg(...)]` on every value-level graph clause — `slot_timeout:`,
  `ack_timeout:`, `beat_timeout:`, `beat_window:`, `ready_on_write`, `disabled`,
  `discover` — and on a single `provides:` entry, joining the existing per-entry
  gates in `deps:`/`resources:`/`reads:`/`writes:`/`dataflow:`. A gated clause
  drops out of the builds where its predicate is off; a gated clause whose
  feature is missing errors only in the builds where it exists. Structural
  clauses reject the gate with an error naming the alternatives, and a gated
  `beat_timeout:` pulls `beat_window:`/`ready_on_write` under the identical
  predicate. Duplicate value-level clauses are parse errors now (last-wins
  would silently drop a gated duplicate's other predicate), and a malformed
  `#[cfg]` is a spanned error rather than a macro panic.
- `init_host_logging(LevelFilter)` (feature `log`, hosted targets): a
  dependency-free stderr sink for simulators and tests, `[uptime] LEVEL
  target: message`, formatted to read like defmt's `timestamp-uptime` output.
  Not on `wasm32-unknown-unknown`, which has no reachable stderr and no
  monotonic clock — install `console_log` there and the records arrive through
  the `log` facade the same way.

### Changed

- The `defmt` backend is embedded-only: the `fmt.rs` arms are gated
  `all(feature = "defmt", target_os = "none")`, and a hosted build with both
  backends enabled routes through `log` instead of failing to link against
  `_defmt_*` symbols. Firmware is unaffected; `cargo test --all-features`
  passes on the host for every crate in the workspace.
- Dependency floors raised to the lock-proven versions (`portable-atomic
  1.15.0` and friends) so the requirements say what is actually tested.

## [0.5.1] - 2026-08-26

No API change; the macro pin stays `embassy-supervisor-macros = "=0.7.0"`.

### Changed
- `bound-deps`: a `ready bound` edge whose readiness budget runs out during
  bring-up now **parks** the dependent (`BOUND_STOPPED`, no spawn, no fault)
  and the wave carries on; the bind loop lifts it when the provider asserts.
  The same applies to a direct `start_node`, so an `Activate` over a down
  provider parks too.

## [0.5.0] - 2026-08-25

Macro pin moves to `embassy-supervisor-macros = "=0.7.0"`.

Runtime-coupling features. `deps:` orders **spawns** — a relation consumed once, at
bring-up. What governs a running system is the continuous dataflow between tasks, and
until now the crate had nothing to say about it: a provider that went quiet after
bring-up was invisible, a node whose readiness meant "reached the line that says so"
rather than "producing" was invisible, and a node restarted underneath its consumers
was invisible. This release adds an opt-in feature family that addresses those, and
one documentation section that draws the line explicitly.

The coupling clauses say what a node exchanges with the rest of the graph. That
declaration is not checked against anything: it is what the heartbeat, the readiness
assertion, the signal-indexed queries and the diagram tool are built on. Where the
body can carry a node, `#[dataflow]` derives the declaration from the code itself,
and then there is nothing to keep in sync.

Everything is off by default. Existing graphs behave identically unless a feature is
enabled and the corresponding clause or marker is used (the featureless `provides:`
clause included: it changes nothing until written).

The same release moves the graph's *structure* into its type. A graph built purely
on runtime coupling declares no `deps:` edges, yet every graph paid for the
spawn-ordering machinery — the dependency table, the stored order, the per-node
gate checks — because a `Supervisor` could not know at compile time what its graph
lacks. Per-node data can never tell it (a `TaskNode` holds atomics, so its statics
are opaque to constant propagation); a `Topology` type parameter carrying the facts
as constants can.

The same lesson is applied one level down, where it buys RAM instead of flash: a
`TaskNode`'s handle atomics forced the whole static into `.data`, dragging the
node's immutable config (name, mode, spawn fn, gates, budgets, coupling tables)
into RAM alongside the dozen bytes that actually change. The node is now split:
the config lives in its own `NodeCfg` static — no interior mutability, so it stays
in flash — and the RAM-resident node holds the handle plus one reference.

The handle itself is packed on top of that: its lifecycle booleans (shutdown,
dropped, running, busy, completed, disabled, detached, ready, bound-stopped) are
bits of one atomic word instead of separate `AtomicBool`s, with the per-flag
`Release`/`Acquire` pairing kept. The per-beat heartbeat flag stays its own byte
so a beat remains one relaxed store.

Measured on the in-repo firmware (thumbv8m, release, a graph that uses deps, pools,
executors and resources — only the absent structures fold): the topology shape bits
save −168 B `.text`, and the node split plus the packed handle move
**−832 B of `.data` (RAM)** to flash — each node drops from 208 B to 104 B of RAM
(trace on; 64 B with a leaner feature set). On a 15-slot edge-free host graph
(`Flat` + shape folds): −928 B across `.text`/`.rodata`/`.data.rel.ro`. A graph now
pays only for the structure it declares, and pays RAM only for what actually
mutates.

### Added

- **`Topology<N>`** — a trait carrying the graph's spawn-ordering side (`deps_of`,
  `order_at`) plus **`SHAPE`**, a bitmask of structural facts (the `shape` module:
  `READY_DEPS`, `EXEC_SLOTS`, `RESOURCES`, `PAUSE`, `ON_DEMAND`, `BEATS`,
  `OBSERVED`, `BOUND_DEPS`, `POOLS`). `SHAPE` is a monomorphization constant, so
  every lifecycle branch serving an absent structure — the pre-spawn executor /
  resource / readiness gates, the parked-`Pause` resume paths, the liveness sweep,
  the observed-write poll, the bound cascade, the pool driver — is compiled out
  when its bit is unset. A set bit only enables code; the macro sets bits
  conservatively (`#[cfg]`-gated items count), and hand-built graphs default to
  `shape::ALL`.
- **`Ordered<N, SHAPE>`** — today's shape as a type: the dep table plus the
  const-computed topological order.
- **`Flat<SHAPE>`** — the topology of a graph with **no** `deps:` anywhere:
  zero-sized. No dependency table, no stored order; walks fold to index loops and
  the dep cascades (`activate`, `deactivate`, `restart`) collapse to their seeds.
  `supervisor_graph!` picks it automatically for edge-free graphs.
- **`Graph::order()`** — iterator over the topological order, replacing the field.
- **`NodeCfg`** — the immutable half of a `TaskNode`, built `const` with `new`
  plus the chainable `with_*` methods (moved here from `TaskNode`). No interior
  mutability, so the `static` carrying it lives in flash; the node references it.
- **`TaskNode::name()` / `mode()`** — accessors replacing the public fields.
- **`epochs`** — a per-node activation generation, `TaskNode::epoch()` /
  `wait_epoch_change()`, bumped on every transition into `running`. Lets a dependent
  that is *already running* notice that a provider was restarted underneath it, which no
  `deps:` edge can do. Being per-node rather than per-edge, it also covers couplings the
  spawn DAG cannot express, including cyclic ones. Pure status: one `AtomicU32` and one
  `Signal` per node, one relaxed load to poll.
- **`liveness-monitor`** — the sweep the `liveness` primitives were missing. New
  `beat_timeout:` / `beat_window:` node clauses, `Supervisor::monitor()` (selected into
  `Supervisor::run` automatically), and `HealthEvent`s delivered through `wait_health()`
  / `try_wait_health()`. Reports a running node that stopped beating, once per stall, and
  reports its recovery. **Report-only**: escalation is domain knowledge the supervisor
  does not have. Only nodes carrying `beat_timeout:` are policed; with none, the monitor
  parks forever and an idle system never wakes.
- **`coupling`** — `reads:` / `writes:` node clauses naming the **actual signal
  statics**, so a path that does not resolve is a compile error and renaming a signal
  breaks every declaration referring to it. `Graph::writers_of` / `readers_of`
  answer the structural questions about one signal — who produces it, who
  consumes it, which pairs are coupled in a loop. A pool counts as ONE
  producer/consumer throughout: its members carry the same tables and dep row, and
  naming any member in `deps:` declares the coupling to all of them. The coupling
  table never feeds the topological sort, so it may contain cycles. Flash-only
  cost, no RAM, no hot-path work.
- **`coupling-observe`** — a way to ask whether a signal moved, and a heartbeat
  built on the answer, asking nothing of the task itself. An `observed` entry
  marker names an accessor whose result changes when the signal is written (`observe writes: <expr>;` /
  `observe reads: <expr>;` set per-direction defaults with `it` bound to the signal;
  `observed via <expr>` overrides one entry; with neither, the entry resolves through
  the `Observable` facade — see `embassy-supervisor-observe` below). The expression
  forms exist because a trait method cannot vary per entry or reach one element of an
  array of signals (`ARR[1] observed`). With `liveness-monitor` the
  sweep turns an advancing `beat` write into the node's heartbeat, so a declaration
  naming the wrong signal makes the node go **stale** rather than merely logging, and
  `ready_on_write` lets the first advance assert readiness. `beat` is the only thing
  that asks today: an `observed` entry without it is inert, declared and never
  called. This is the tier for a
  body you cannot change — a vendor driver, a generated task — since no node reaches
  the task's signature. Where the body can be changed, `dataflow` does both
  jobs at the access instead of at sweep resolution. Costs one `Option<fn() -> u32>`
  per entry (niche-packed to one word) and one `AtomicU32` per node.
- **The `beat` qualifier** (`writes: [X observed beat]`) distinguishes a declaration
  that merely states a coupling from one that is also the node's sign of life. A node
  with several outputs can therefore declare all of them while only the one whose
  silence means "wedged" reports liveness — a heartbeat that meant "any of my outputs
  moved" would not. `ready_on_write` requires a `beat` entry, since it fires from the
  same place, and `beat` on a `reads:` entry is rejected: a heartbeat is something a
  node produces.
- **`ready_on_write`** — a node clause (features `coupling-observe` + `readiness`) where
  the sweep asserts the node's readiness the first time one of its `observed` writes
  advances, so "ready" means actually producing rather than reaching the line that says
  so. Requires a `beat`-qualified entry in `writes:` and, in the polled form,
  `beat_timeout:` —
  which is what puts the node in the sweep; either alone would be a silent no-op and
  both are compile errors. On a bare `beat` write the assertion happens at the
  report call instead, and needs no budget. Monotone, and never withdrawn: a node that
  goes quiet is reported through `wait_health()` and what that means stays the
  application's decision.
- `reads:` / `writes:` entries accept a per-entry `#[cfg(..)]`, as `deps:` entries do,
  and an optional `[idx]` selecting one element of an array of signals. Declaring the
  same path both bare and indexed is a compile error: `&ARR` and `&ARR[0]` share an
  address, so nothing downstream could tell them apart.
- `Coupling` is built rather than constructed literally (`Coupling::new(..)`,
  `.observed(..)`, `.beat()`, with `name()` / `observer()` / `beats()`), so a
  later field cannot break every graph that names a signal.
- **`dataflow`** — the explicit tier beside `coupling-observe`'s implicit one,
  the split AUTOSAR ships as `Rte_Write` beside `Rte_IWrite`: the node is the access
  path, and the access is the record. The **`#[dataflow]` attribute** on a task fn
  derives its coupling tables at compile time from the verb calls in its body —
  `put(&SIG, v)` / `get(&SIG)` perform the write or the snapshot read themselves
  through the facade's `Sink`/`Source` traits, `writer(&SIG)` / `reader(&SIG)` hand
  the typed signal back for read-modify-write and per-consumer-handle patterns — and
  rewrites each call site to carry its flash table entry, so the record cannot drift
  from the code. A derived table states couplings and nothing else, and a node's sign
  of life rides on the write that proves it. The scan is receiver-keyed on the fn's
  `TaskNode` parameter (a `map.get(&key)` is never touched); computed arguments
  are span-attached errors.

  **Verbs of your own.** The verbs are inherent methods on `TaskNode`, so an extension
  trait could always add more; what the scan needed was the ident and its direction.
  `#[dataflow(read(subscribe), write(publish))]` supplies both, and the arguments carry
  nothing else. A registered verb takes `Sig<T>`, which is where the rewrite puts the
  table entry, and hands the signal back with `s.target` as `reader` does; its accesses
  then reach the derived tables, the signal-indexed queries and the diagrams exactly as
  the built-in verbs' do. Registrations are additive (naming a built-in verb is an error
  rather than a silent redefinition) and per fn (the same method is an ordinary call
  where it is not registered, its coupling simply absent from that fn's tables).
  Direction is stated rather than inferred because the scan is token-level and has no
  types to ask, and it is not cosmetic: `writers_of`/`readers_of`, the heartbeat and the
  gate's producer lookup all partition on it. Compile-time only, so a registered verb
  costs nothing a built-in one does not. `supervisor-mermaid` reads the same attribute
  through the shared walker and draws a registered verb under its own name, so the build
  and the diagrams cannot disagree about what a verb is.

  The tables bind two ways, one per source: a **`discover`** node takes its task
  fn's derived tables in place of `reads:`/`writes:` lists, and **`dataflow: [..]`**
  adopts named accessor fns' tables beside whatever the item already has — an
  accessor takes the caller's node, so a fully private signal's one write path can
  be exported as a fn and its accesses attribute to whoever calls, with the static
  never leaving its module. **`#[dataflow_bundle]`** rolls an inline module's
  `#[dataflow]` fns into one adoptable table pair (`dataflow:
  [crate::api::BUNDLE]`) — exactly the members' entries, concatenated, a member's
  own `#[cfg]` gating what it contributes. A node's coupling is therefore a table
  per source (declared list, task fn, each adopted fn or bundle);
  `TaskNode::reads()`/`writes()` return the nested shape. Everything is flash-const: the signal queries and the diagram
  tool see the derived edges. A derived table states couplings and marks none, so a
  `reads:`/`writes:` list may sit beside `discover` **to add markers only** —
  every entry must carry `observed`/`beat` and name a signal the scan already
  found, checked by an emitted const assertion over the bound tables (matched on
  the path's last segment, since a const context cannot compare addresses). That
  is how a `discover` node carries a heartbeat and `ready_on_write`. `<signal> beat` in a list asserts `ready_on_write` inline with no
  `beat_timeout:` involved, and a `discover` node beats and asserts readiness from
  its body. Independent of `coupling-observe`; an entry may carry both markers.

  The heartbeat self-silences, so the verbs stay cheap at any message rate: it is a
  flag that whoever next checks staleness converts into a beat inside
  `ticks_since_beat` — the checker pays the clock read it was making anyway, so a
  high-rate writer pays a relaxed store per message, never a timer read (readiness is
  the exception, asserted at the access, since bring-up waits on it). Steady state
  per write: a scan of the node's entries, short-circuited at the first `beat` entry,
  plus that store; reads are pure pass-throughs. RAM: one flag byte per node; every
  table is flash.
- **`node-status`** — `TaskNode::report_status("receiving image")` / `status()`, the
  `sd_notify(STATUS=..)` verb: a one-line self-description shown when asked (a
  dashboard, a log line on change), never an event, never acted on, and cleared on
  activation so a fresh instance does not wear the previous one's last words. Costs one
  mutexed cell per node.
- **`embassy-supervisor-observe`** — a new leaf facade crate in the `log` mold,
  depending on nothing but `portable-atomic`: a signal library implements its traits
  where the signal type lives, without depending on the supervisor — the layering that
  made a supervisor-side trait impossible. It defines `Observable`
  (`change_token(&self) -> u32`), the value contracts `Sink`/`Source` behind the
  supervisor's `put`/`get` verbs, and `Counted<T>` (a wrapper counting `.w()`/`.r()`
  accesses, so even a same-value rewrite registers, forwarding `Sink`/`Source`
  through the counted handles). The atomics — both the core and portable families —
  implement all three out of the box. The supervisor re-exports the traits, and a bare
  `observed` entry with no `via` and no graph default now resolves through
  `Observable` instead of being a "needs an accessor" error.
- **`restart`** — `Supervisor::restart()`, Erlang/OTP's `rest_for_one`: stop the target's
  transitive dependents in reverse topological order, respawn the target, bring the
  dependents back through the **full pre-spawn gate sequence**, so a `ready`-marked
  provider must re-assert readiness before any dependent returns. Distinct from
  `deactivate` + `activate`, which additionally clears `disabled` on the target's
  *dependencies*, swallows spawn errors, and latches visible state between the calls.
- **`bound-deps`** — the `bound` dep marker (`deps: [X ready bound]`). See *Changed*.
- **`log`** — routes the crate's own log macros through the `log` facade, the way `defmt`
  already did for firmware. Only one backend can be live (each `{}` resolves through a
  different trait), so `defmt` takes precedence when both are on rather than erroring —
  `--all-features` enables both, and that is what docs.rs builds. This closes a gap
  that left every hosted consumer with no supervisor output at all: the
  `liveness-monitor` stale reports, and every bring-up and teardown line, went
  nowhere. `Mode` gained `Display` to match its `defmt::Format`, for the one call
  site that formats it.
- **Data-driven dependencies** (feature `data-deps`): `TaskNode::open(&SIG).await` reads a signal
  through the signal's own `Gated::ensure`, so a coupling that is depended on rather
  than merely observed states its precondition once, at the declaration, instead of
  being restated as a `deps: [PRODUCER ready]` edge in every consumer. `Backed<T>`
  wraps a signal whose producer is started on first open and whose reader waits for
  that producer's readiness; nothing names the producer, because `producer_of` finds
  the node that declares the write by address, which covers `discover`-derived tables
  a declaration site could never name. `open` is the only awaiting verb: a gate fires
  once per consumer at setup, so the future belongs there and not on every access, and
  it grants no exclusive access. `Deref` keeps every existing consumer of a wrapped
  signal compiling unchanged, which also means the gate is advisory for code that
  reaches the static directly. Costs nothing for an ungated signal: no wrapper, no
  state, no code, and `open` on one is a compile error rather than a silent no-op.
  Starting the producer needs `control`, the mailbox the request goes through; without
  it the gate is the readiness wait alone, which is right for a boot-started producer
  and reported (once per down cycle) for any other, since nothing would start a `disabled`
  or `OnDemand` node and `wait_ready` is unbounded.

  The same coupling's other edge, on the way down: `Leased<T>` and `TaskNode::lease`.
  A producer that published a handle cannot free it while a consumer still holds one,
  and no declaration answers that — `reads:` records that a node touches a signal,
  never that it holds something derived from it across an await, and a coupling table
  is best-effort besides. So the holders are counted: `lease` hands out a guard,
  dropping it drops the count, and `Leased::drain` closes the signal to new leases and
  waits for zero. Closing is what makes the count trustworthy; a consumer asking
  afterwards gets `None` rather than a handle about to dangle. Exact where a
  declaration is best-effort, which is the point: an undeclared access, an unadopted
  helper, and a `detached` node teardown never touches all hold a lease or they do not.
  A leaked guard surfaces as the producer's ordinary ack timeout. One `AtomicU32` and
  one `Signal` per leased signal, nothing for the rest.

  A feature of its own rather than part of `dataflow`, because it gives every node a
  back-pointer to its own graph (`TaskNode::graph`) — 8 bytes of RAM per node,
  which a graph with no gated signal never uses (measured: `TaskNode` 96 bytes without
  the feature, 104 with, at alignment 8). Everything else is const: `producer_of` searches the graph
  the reading node belongs to, so there is no registry to size, no bound on how many
  graphs a firmware may hold, and a gate resolves before `Supervisor::start` as readily
  as after.
- **The derived tier follows statement-level `#[cfg]`**: a verb call inside a
  feature-gated statement emits a coupling entry carrying the same gates, so a
  feature-modular body derives cleanly instead of keeping such reads out of the
  tier by hand.
- **`deps:` is optional** (nodes and pools): omitting it is `deps: []`, the natural
  spelling for a graph whose ordering lives entirely in the runtime coupling —
  gated reads, lease drains, channel rendezvous — which the lifecycle waves bring
  up as one round.
- **The `provides:` node clause** (no feature — the clause is its own opt-in, like
  `slot_timeout:`) names the resource slots a node's task fills at runtime, and the node's shutdown ack clears them: the value is
  dropped and the filled latch reset, `Pause` parks excepted (a parked task still backs
  what it published). This is what makes "filled" mean "valid" for a `shared` slot,
  which its consumers never empty — after its provider stops, a gate wait would
  otherwise hand out the previous activation's handle, and resources are not couplings,
  so nothing else links a slot to the task that fills it. With the clause, emptiness is
  the freshness signal the existing gate waits already understand, and clearing from
  the ack also covers providers that ack inside `run_cancellable_acked` or a `cancel`
  shell (no code point before the ack) and autonomous exits through `mark_exited`.
  `ResourceSlot::clear()` — with a default-no-op `ResourceGate::clear` behind it — is
  the manual form. Costs one always-present slice pointer per node (absorbed by
  `TaskNode`'s padding on the measured 32-bit configuration) and, per provider, the
  gate array in flash.
- **`graph-ref`** — a graph as one addressable `'static`. `supervisor_graph!` emits a
  `GraphRef` beside the node table and `GRAPH.graph_ref` names it. A graph was otherwise
  only a set of statics, and two features need to refer to the graph itself, in opposite
  directions: `data-deps` needs node → graph, to search a node's own peers, and `trace`
  needs binary → graphs, because a hook holds an opaque task id and no node at all. Both
  now pull this, and it carries neither of them — only the handle. The chain `trace` links
  each `GraphRef` into costs one reference and one flag per graph, so it is `trace`-gated:
  with `graph-ref` alone a `GraphRef` has no interior mutability and lives in flash.
- **The heartbeat is a verb, not a declaration.** `beat_put` / `beat_writer` are
  `put`/`writer` that also beat: the claim "I published, therefore I live" is made at
  the write that makes it true, where it cannot drift from the publish under
  refactoring, and the derived table records which write carries it. A bare `beat`
  entry in a `writes:` list is rejected — `beat` survives only as the qualifier on
  `observed`, where there is no call site to carry it because the supervisor cannot see
  into the body. `ready_on_write` follows it into the polled tier; a body that beats
  through its verbs asserts its own readiness with `set_ready()` at the same write.
  Both verbs are gated on `liveness`, the feature that gives a node a heartbeat at
  all, rather than degrading to `put`/`writer` without it: a heartbeat verb that does
  not beat is a claim the build silently does not make, and a node that never beats
  reads as live forever. Inside `#[dataflow]` the rejection is span-attached and names
  the feature and the plain verb to fall back to. `put`/`get`/`writer`/`reader` are
  unaffected.
- **`TaskNode::beat` costs one relaxed store**, at any rate. It raises a flag that
  whoever next asks about staleness converts into a timestamp inside
  `ticks_since_beat`, using the clock read that call makes anyway, so a high-rate
  publisher never touches the timer. The consequence to know: the recorded
  instant is "when a checker next looked after a beat", not "when the beat happened".
  That is the question staleness asks; it is not a latency measurement.
- The `#[dataflow]` walker carries the **verb**, not just the direction, into the
  derived tables' consumers. `supervisor-mermaid` draws a gated read (`open`) and a
  heartbeat write (`beat_put`/`beat_writer`) as their own edges, so a diagram shows
  what kind of coupling each edge is and not merely its direction.
- Graph introspection, always available: `Graph::index_of`, `deps_of`, `dependents_of`,
  `iter_nodes`; `TaskNode::slot_timeout()` and `ready_deps()`. These are what an
  app-owned health view needs to get from a node to its place in the topology.
- Crate docs and README gained a **"Spawn ordering is not runtime coupling"** section
  tabulating what each declaration relates and when it applies, and documenting the
  app-owned health-monitor pattern.

- **BREAKING: `Spawner` rides by reference.** Every entry point that spawns —
  `start`, `run`, `run_pools`, `start_node`, `respawn_terminate`, `activate`,
  `apply_control`, `apply_bind`, `restart` — now takes `&Spawner`. `Spawner` is a
  two-word `Copy` handle that every nested driver frame stored for its whole
  lifetime; a reference halves that in each frame of the host task's storage.
  Migration is mechanical: `sup.run(spawner)` becomes `sup.run(&spawner)`. The
  `NodeCfg` spawn fn type still takes `Spawner` by value.

- **Respawn no longer races embassy's two-phase task cleanup.** A task woken during
  its final poll exits with its run-queue entry still armed, and the storage is
  claimable only one executor pass later; a wave respawning in that window got
  `SpawnError::Busy` from a logically free slot (observed as `restart(net)` failing
  ~100 µs after its spawn). The waves now treat a failed spawn as one more
  unsatisfied gate — yield a pass and retry, faulting once the node's slot budget
  is spent — and `start_node` retries once after a yield, so a still-`Busy` result
  remains a real pool ceiling.

- **Resources never cross the spawn call.** The glue used to `take()` lend/consume
  values and move them through the task-fn call, where a `Busy` claim dropped them
  unrecoverably (the demo's `net` lost its USB `Peri` this way). The glue now only
  probes the slot (fail-closed `SpawnError::Busy` for unprovided slots, unchanged)
  and the generated shell reads it at first poll — `take()` for lend/consume,
  `get()` for `shared`, which also stops `Copy` handles from sitting in the task
  arena as arguments for the whole run (rust-lang/rust#62958). A shell that loses
  the probe-to-poll race to an out-of-band `take()` warns and records a clean
  instant exit instead of panicking. `pool_size > 1` with lend/consume entries is
  now a macro error (the slot holds one value; use `shared` or an `ElasticPool`).

- **`trace-self`: the supervisor's own host task as a hidden node.** Each `GraphRef`
  carries one extra `"supervisor"` `TaskNode` (outside the node table, so waves,
  the monitor and respawns never see it), and `start()` stamps the calling task's
  id into it — no declaration needed. The waves, driver loop, monitor and control
  application then show up in per-node poll accounting instead of the unsupervised
  share. Attribution is task-granular: everything else the host task polls is
  billed to it too. Read it via `GraphRef::self_node()`. Costs one `TaskNode` of
  RAM per graph, only with the feature on.

- `TaskNode::adopt_current()`: bind the **calling** task to a node from inside its
  body — the token-free counterpart of `adopt` for parked and verbatim-closure
  nodes, built on the new `trace::current_task_id()` (the task id read from the
  task's own waker).

- `run_cancellable`/`_acked`'s future slimmed: shutdown is polled inline off the
  node instead of embedding a `wait_shutdown()` state machine, saving ~8 bytes of
  task storage per live worker.

- **Lifecycle waves, built in** — both lifecycle directions run concurrently, with
  the order guarantees intact; there is no feature gate and no sequential fallback.

  Down: every stop path — `teardown`, `teardown_continue`, `deactivate`, `restart`'s
  down half and the `bound-deps` cascade — signals a node the moment every dependent
  stopping with it has acked, signals nodes with no such dependents up front, and
  re-runs the scan on each ack. A `deps:` dependency therefore keeps serving until its
  stopping dependents are gone — a dependent may flush over a link, or drive one last
  ioctl through a runner it depends on, during its own shutdown — while a node whose
  own shutdown waits on a node it has *no* edge to is signalled without queueing.
  Signalling one node at a time cannot survive that wait: a producer draining a
  `Leased` signal holds its ack until its consumers let go, and the supervisor holds
  those consumers until the producer acks. Nothing breaks that circle, and it ends as
  a shutdown-ack timeout.

  The contract a node writes against: an *unordered* node — no `deps:` path between
  them — may be told to stop while other nodes are still running; it stops serving
  immediately and frees what it owns as soon as it acks. A node that publishes a
  handle to consumers it has no edge from must hold its own shutdown until they have
  let go (`Leased` + `drain`) rather than rely on being asked last, and what a node
  uses during its own shutdown must be one of its `deps:` — an unordered service
  cannot be assumed to still work. Nodes that only ack and exit are unaffected.

  Up: `start`, `respawn_terminate`, `activate` and `restart`'s up half spawn every
  node whose in-pass deps are up and whose gates test satisfied on each round, parking
  between rounds on a gate-event signal fired by `provide`/`restore`,
  `SpawnerSlot::set` and `set_ready`. Independent slow bring-ups overlap instead of
  queueing, and a provider may be declared after its consumers with no dep edge
  (pinned by `tests/bringup_concurrent.rs`); spawn ordering is strict — a dependent
  never spawns before its in-pass deps. The non-blocking gate test the wave stands on
  is *emptiness*, which is what `provides:` (or a manual `ResourceSlot::clear()`
  before the ack) guarantees for provider-rebuilt `shared` slots. A node's
  `slot_timeout` covers all its gates together, from when its deps resolve; the
  `bound-deps` reconciliation's restart side stays per-node, since it is re-driven on
  every readiness transition anyway.

  Migration notes for graphs written against 0.4's one-node-at-a-time walks: a node
  relying on incidental reverse-declaration order against a node it has no edge to
  must declare the edge; a provider-rebuilt `shared` slot must be covered by
  `provides:` (or cleared before the ack); `activate` on a down-but-not-parked
  `Pause` node now spawns it through the gates instead of blind-resuming into a
  ghost; and a control `Activate` of a stopped-at-boot node goes through the same
  spawn path.
- `ack_timeout: MS` graph clause (node and pool) and `NodeCfg::with_ack_timeout` /
  `TaskNode::ack_timeout`: per-node override of the 2 s shutdown-ack window. Both stop
  paths honor it — the single-node wait and the whole-graph wave, where each node's
  window runs from the moment it is signalled. Raise it for a node whose cleanup
  legitimately outlasts the default; a missed ack still faults with
  `FaultKind::ShutdownTimeout`, just later.
- `TaskNode::run_pausable` and `TaskNode::run_pausable_loop`: the `Pause`-mode
  combinators. `run_pausable` races the work against shutdown like
  `run_cancellable_acked`, and when a pause wins it drops the worker, acks, parks, and
  returns `Err(Resumed)` (new marker type) only once the supervisor resumes the node —
  the caller's loop body is the fresh cycle, and the ack/park sequence can no longer be
  forgotten or misordered. `run_pausable_loop` takes an `AsyncFnMut` body and owns the
  loop too: one call, never returns. Both hold the worker future once, like the
  cancellable pair.
- `state: zeroed Type` (feature `heap-state`): per-activation state allocated
  zero-filled, with no init value built first. `state: Type = init_expr` constructs the
  value in the spawner's frame and copies it into the Box unless the optimizer elides
  the copy, so a large buffer set transiently costs its size in stack; the zeroed form
  never does. `Type` must implement `Zeroable`, re-exported from `bytemuck` as
  `embassy_supervisor::Zeroable` (the feature now pulls `bytemuck` with no features).

### Changed

- **Breaking:** `Graph<N>` and `Supervisor<N>` gain a `T: Topology<N>` parameter
  (defaulted to `Ordered<N, { shape::ALL }>`). The `deps` and `order` fields are
  replaced by `topo`; read edges through `deps_of` / `dependents_of` / `order()`.
  A `static` supervisor names the macro's emitted alias:
  `static SUP: Supervisor<5, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);`.
- `Graph::deps_of` is no longer `const` (it delegates through the trait).
- **Breaking:** `TaskNode` is the handle plus a `&'static NodeCfg`. Its `name`,
  `mode` and `spawn` fields are gone — read `name()` / `mode()`; the `with_*`
  builders moved to `NodeCfg`, and `TaskNode::new(cfg, disabled_at_boot)` takes
  the config by reference. A hand-built node is now two statics (macro-declared
  graphs are unaffected — `supervisor_graph!` emits both).
- **The trace recorders resolve through any number of graphs** (breaking:
  `trace::MAX_GRAPHS` is removed and `trace::register_graph` takes a
  `&'static GraphRef` — `GRAPH.graph_ref` — in place of the node slice). Registration
  was a four-slot table that silently dropped the fifth graph, so its nodes simply never
  accumulated a poll: an id that resolved to nothing is indistinguishable from an
  unsupervised task. It is now an intrusive chain through the `GraphRef` the macro
  already emits, which is a push with no cap. Per-poll cost goes from one critical
  section for the whole table to one for the chain head plus one per graph — two for
  the single-graph case — against an id scan that was already O(total nodes).
  Applications calling `Supervisor::start` are unaffected; only a graph registered by
  hand needs the new argument.
- **One error type for every lifecycle failure.** `NodeFault { node, kind: FaultKind }`
  replaces `ShutdownTimeout` and `RunError`, and is returned in place of embassy's
  `SpawnError` by `start`, `start_node`, `respawn_terminate`, `teardown`,
  `teardown_continue`, `stop_node`, `deactivate`, `activate`, `run`, `restart` and
  `run_pools`. `FaultKind` names what actually went wrong — `ExecutorSlotEmpty`,
  `ResourceMissing`, `ReadyDepTimeout { dep }`, `Spawn(SpawnError)`, `ShutdownTimeout`
  — where the four bring-up causes all arrived alike as `SpawnError::Busy`, whose text
  sends the reader to `pool_size` and fits only the last of them.

  `Display` is implemented **unconditionally** (none of the old types had it, so callers
  fell back to `{err:?}` and got embassy's string): a bring-up failure now reads
  `att-estimator: ready-dep imu-reader did not assert within 2000ms`. `run`'s two-arm
  match collapses to `panic!("{}", sup.run(&spawner).await)`, and `RunError` no longer
  loses the node. `TaskNode::spawn` keeps embassy's signature — that is its own contract.
- `Supervisor::monitor` wakes when the earliest node is next due rather than on a global
  period. The period was `min(beat_timeout)/2` across the whole graph and every wake
  examined every node, so one tight budget set the rate for everyone — a node with a 5 s
  budget alongside one with 500 ms was examined twenty times more often than it needed.
  Per-wake work is unchanged; only the cadence is. A node still waiting to assert
  `ready_on_write` is probed at `beat_timeout / 8` instead, because readiness gates
  dependents against their `slot_timeout` and noticing the first write late spends
  someone else's bring-up budget; an overdue node is re-examined every half budget, so
  `beat_window` strikes accumulate at a bounded rate. `ready_on_write` remains pull-based
  and so is never faster than calling `set_ready()` from the task.
- A ready-dep timeout is no longer logged by a `warn!` inside the wait; the returned
  fault carries it. Only `activate` still logs, because it is best-effort and discards
  the fault.
- **The default feature set is now just `macros`**, down from
  `["control", "pool", "macros"]`. `control` and `pool` are capabilities rather than
  core, and both add driver-loop code that runs every iteration whether or not a graph
  uses them — a graph with no pool and no control ops was paying for both by default.
  Measured on a consumer that uses neither: **1424 bytes of `.text`**. `restart` and
  `bound-deps` still pull `control` in themselves, so most consumers of those need no
  change; a graph declaring a `pool` must now name the feature, which the macro reports
  as a spanned error rather than a silent behaviour change.
- **`ControlOp` is now `#[non_exhaustive]`** (breaking for exhaustive matches) and gains
  `Restart` under the `restart` feature.
- **`clear_ready` is no longer unconditionally "status, not control".** It still is for
  every edge as written today, and remains so by default. With `bound-deps`, an edge
  marked `bound` propagates: the provider's `clear_ready()` stops that dependent
  (transitively along bound edges, reverse topological order, without latching
  `disabled`), and its `set_ready()` brings back every node so stopped whose bound
  providers are serving again, through the full gate sequence. A manual `deactivate`
  outranks the cascade, and `TaskNode::is_bound_stopped()` distinguishes a bound stop
  from a deliberate one. **Wrong for a chain that cannot tolerate a stop-restart window**
  — pair `liveness-monitor` with `epochs` there instead.
- The scope docs now state plainly that the supervisor does not check declarations
  against each other, and that the monitor polices self-reported beats, not progress.

### Fixed

- A node declared with `exit:` at a `compose_graph!` site failed to compile
  (`cannot find value __out in this scope`) whenever fragments were also present: the
  shell's `let __out` and the exit slot's `provide(__out)` were emitted with different
  hygiene contexts. Both now carry the `exit:` type's span, which also keeps the
  clause's `unreachable_code` diagnostic pointing at the clause. See macros CHANGELOG.
- `Supervisor::resume_pausable` now skips nodes without a parked instance, as
  `resume_node` always did. Sweeping a running `Pause` node latched its resume signal
  with no waiter, and the node's next park consumed the stale latch as a spurious
  resume: the task kept serving while the supervisor had it marked down.

## [0.4.3] - 2026-08-04

Static-RAM fix for cancellable workers, on both sides: the runtime combinators and
the generated `cancel` shells (macro pin moves to
`embassy-supervisor-macros = "=0.6.2"`). No DSL, API or behavioural change —
recompiling picks it up.

### Changed
- New dependency `pin-project-lite` (declarative-macro only, `no_std`) for the safe
  pin projection the hand-written future needs.
- **MSRV raised to 1.88** (from 1.85), for the let-chain in that future's `poll`.

## [0.4.2] - 2026-08-03

Macro pin moves to `embassy-supervisor-macros = "=0.6.1"`. No runtime code, API,
or feature change on this crate.

### Fixed
- `cancel` on a `task:` written as a partial call (`task: worker("arg")`) with no
  `resources:`/`state:` failed to compile. See the macros CHANGELOG.

## [0.4.1] - 2026-08-03

Ships the `cancel` graph flag by moving the macro pin to
`embassy-supervisor-macros = "=0.6.0"`. No runtime code, API, or feature change on
this crate.

### Added
- `cancel` flag in the graph DSL, on `task:` nodes and on `task:` pools (macros
  side, feature `macros`; no runtime change and no new API): the generated shell
  owns the shutdown race via `run_cancellable`, so a plain supervisor-unaware
  `async fn` binds directly — no node argument, no handshake in the worker, and
  the shell still frees state, restores resources and records the exit when the
  future is dropped in place. On a pool it covers every member, so an elastic
  shrink can retire such a worker. The flag requires `task:` and rejects `Pause`.
  See the macros CHANGELOG and the README's "`cancel` — supervisor-unaware
  workers" section.

### Changed
- `exit:` on a worker that can never return is now a compile error (macros side)
  rather than a slot nothing could ever fill. See the macros CHANGELOG.

## [0.4.0] - 2026-08-02

Defect fixes: the control mailbox can no
longer drop a request silently, a task body that returns on its own is now observed,
and a missed shutdown ack is an error the application escalates instead of a panic
inside the supervisor. Macro pin moves to `embassy-supervisor-macros = "=0.5.0"`.

### Changed (breaking)
- `request_control` is now `async` and lossless: it awaits mailbox capacity instead
  of silently dropping when the 4-deep channel is full. The old fire-and-forget
  behavior is gone; for sync contexts (ISRs, callbacks) use the new
  `try_request_control`, which returns `Err(ControlQueueFull)` instead of dropping.
- The shutdown paths no longer panic on a missed ack. `stop_node`, `teardown` and
  `apply_control` now return `Result<(), ShutdownTimeout>` naming the offending
  node; `run_pools` returns `ShutdownTimeout` (it only completes on error, from a
  shrink whose member missed its ack). `teardown` aborts the cascade at the first
  timeout so a still-live dependent never has its dependencies stopped under it;
  the previous behavior is one token away (`.unwrap()` / `defmt::unwrap!`).

### Added
- `TaskNode::mark_exited()` / `TaskNode::has_exited()`: a task body that returns is
  now recorded as completed (and the teardown handshake acked). The generated
  `task:` shell calls `mark_exited()` automatically after the worker returns and
  resources are restored, so an autonomous exit no longer reads as running forever
  and a control `Activate` can respawn it. Hand-written `spawn:` tasks call
  `mark_exited()` where they previously called `ack_dropped()` on exit.
  `has_exited() && !shutdown_requested()` distinguishes an autonomous completion
  from an acked stop.
- `Supervisor::teardown_continue()`: best-effort teardown for the
  "hardware reset next" escalation — visits every node in reverse order past a
  non-acking one and returns the first `ShutdownTimeout` at the end.
- `ControlQueueFull` and `ShutdownTimeout` error types (`defmt::Format` under the
  `defmt` feature).
- **`readiness` feature** (off by default): task-asserted readiness. Providers call
  `set_ready()` once actually serving (DHCP bound, registration done);
  `deps: [NET ready]` then holds a dependent's spawn until the assertion (bounded by
  the dependent's `slot_timeout`, then `SpawnError::Busy` with a log line naming the
  not-ready dep — plain deps still order spawns only). Elastic-pool growth defers
  while a ready-marked dep is un-ready. `clear_ready()` withdraws readiness as
  status, not control (dependents are never stopped); the pre-spawn reset clears it
  so a respawned provider re-asserts. Costs one AtomicBool + one Signal + one slice
  per node.
- **`liveness` feature** (off by default): per-node heartbeat. Bodies call `beat()`
  per work loop; an app watchdog reads `is_stale(max_age)` to catch
  alive-but-wedged tasks (parked on an await that will never complete) — the
  complement of the `trace` stall watermark, and independent of `trace`.
  `set_running` stamps a beat so a fresh spawn is never instantly stale;
  not-running nodes are never stale. Costs one AtomicU32 per node.
- `TaskNode::run_cancellable(fut)` / `run_cancellable_acked(fut)`: the select
  against `wait_shutdown()` as a combinator — `Ok(output)` on completion,
  `Err(Aborted)` when a stop wins; the `_acked` variant completes the handshake
  before returning, for bodies with no cleanup between the select and the ack.
- **Named multi-graphs**: `name: IDENT;` as a graph's first item (and
  `compose_graph! { name: X, … }`) renames the emitted static and suffixes every
  generated helper, so several supervisors coexist per binary — the shape a
  subordinate sub-graph wants (a dedicated graph the app state machine cycles
  with `start()`/`teardown()` per phase). The unnamed graph is the primary: only
  it emits the `trace-hooks` symbols; the trace registry now tracks up to
  `trace::MAX_GRAPHS` graphs. The control mailbox is shared — run ONE driver and
  apply each command to every supervisor (foreign-node commands no-op safely).
- `Supervisor::resume_node(node)`: resume ONE `Pause` node parked by an earlier
  `stop_node`/`teardown` — the single-node partner of `resume_pausable` (same
  sequence, same deliberate absence of dependency gating), completing the
  node-level verb set: `stop_node` is the single-node pause for a `Pause` node,
  `resume_node` the other half. No-op unless the node is `Pause`, actually
  parked, and neither disabled nor detached.
- `Supervisor::activate(node, spawner)` / `Supervisor::deactivate(node)` are now
  public — the cascading, `disabled`-latching subsystem verbs, previously
  reachable only by wrapping a `ControlCommand` for `apply_control` even when
  holding the supervisor directly. `apply_control` remains the mailbox-dispatch
  form. `activate` returns `()` (cascade spawn errors are deliberately
  swallowed and re-driven); `deactivate` returns `Result<(), ShutdownTimeout>`.
- `Supervisor::run(spawner) -> RunError`: the canonical driver as one call —
  `start()` then drive pool scaling and runtime control forever, returning only
  on error (`RunError::{Spawn, Shutdown}`). The manual
  `select(run_pools, wait_control)` loop remains for apps with extra wake
  sources.
- `start()` is now the universal quiescent-to-running op, so
  `start()`/`teardown()` cycles on a (sub-graph) supervisor handle every mode:
  each node is reset before its spawn (as `start_node` always did), running and
  detached nodes are skipped (idempotent; a detached instance survived the
  teardown), and a `Pause` instance parked by an earlier teardown is resumed in
  place instead of double-spawned — the completion flag (`mark_exited`) is what
  makes "parked" (`acked && !completed`) distinguishable from "exited". The
  resume path bypasses the gate waits like `resume_pausable` (a parked instance
  retains its resources, so its slots are empty by design).
- **`heap-state` feature** (off by default): the `state: Type = init_expr` clause
  on `task:` nodes and pool members — reclaimable per-activation heap state. The
  spawn glue fallibly boxes the init value (alloc failure = `SpawnError::Busy`,
  nothing spawned or stranded, retryable), the shell lends the worker `&mut Type`
  and drops the Box on task exit, before restores and the completion record —
  every activation allocates fresh, every exit frees, net zero across respawns.
  Task STORAGE stays static by design: embassy wakers are unrefcounted pointers
  into it, so freeing it is unsound — heap only where it can be
  reclaimed. The runtime crate stays no-alloc/`forbid(unsafe_code)`; the ~6-line
  fallible-boxing helper (the feature's entire unsafe surface) is emitted into
  the consumer crate, like `local-resources`. Consumer needs a
  `#[global_allocator]`.
- **Composable graphs**: `supervisor_fragment! { name: X; <items> }` lets a module
  or a whole crate declare its slice of the graph; `compose_graph! { fragments:
  [X, ::other::Y], graph: { .. } }` assembles them into ONE `supervisor_graph!`
  expansion — cross-fragment deps resolve by name in either direction, and every
  compile-time pass (name map, u8 slots, topo order, shared-slot dedup, 256 cap)
  checks the whole composed graph. Errors are attributed to the owning fragment.
  Fragment paths use `$crate::…`; `#[cfg]` inside a fragment evaluates against the
  compose crate's features (documented).
- **Per-member pool resources**: pools now accept take-kind `resources:`
  entries (default lend and `consume`), emitted as per-member slot arrays
  `[ResourceSlot<T>; K]` — member `I` takes/restores element `I` exclusively, the
  floor comes up with floor-many elements provided, and a lend value survives
  shrink/regrow on the same index. `shared` stays pool-wide — `shared local`
  included, as in 0.3.x; only take-kind `local` on pools stays rejected (the
  single-core slot contract + per-member restore is deferred). New
  `ElasticPool::member_index(node)` lets a worker index per-member app state.
- **Const-expression pool bounds**: `min:`/`max:` accept any const-evaluable
  `usize` expression; non-literals make the emitted `_MIN`/`_MAX` consts the source
  of truth, guarded by const asserts (min <= max <= members <= 255). The member
  count (the mode list) stays a literal: it drives how many items are emitted,
  which a proc macro cannot derive from a const.
- `exit: Type` graph clause (`task:` nodes): the worker's return value is
  `provide()`d into a generated `pub static <NODE>_EXIT: ResourceSlot<Type>` just
  before the completion is recorded, so `has_exited()` implies the value is
  readable. `ResourceSlot::wait_take()` awaits and takes it. Idiom: a worker
  returning `Result<R, Aborted>` straight out of `run_cancellable` records
  completed-vs-cancelled.

## [0.3.5] - 2026-07-27

RAM saving for the `Supervisor` itself: the topological order is now borrowed from
the `static` graph instead of copied into the struct. No API, feature, or behavior
change; the macro pin is unchanged (`embassy-supervisor-macros = "=0.4.1"`).

### Changed
- `Supervisor<N>` holds `&'static [u8; N]` for the precomputed order rather than an
  inline `[u8; N]`. A supervisor usually lives inside a task future — i.e. in that
  task's `static` storage — so the copy cost N bytes of RAM per supervisor, plus the
  code to make it, for no benefit: `Supervisor::new` already takes `&'static Graph<N>`,
  so the array it copied from outlives the supervisor by construction. Net saving is
  `N - 4` bytes on 32-bit targets. The field is private and `new` is unchanged, so this
  is source-compatible.

## [0.3.4] - 2026-07-09

Safety fix to 0.3.3 (macro pin -> `embassy-supervisor-macros = "=0.4.1"`): using the
`local` resource kind now requires the new **`local-resources`** feature, OFF by
default. `local` is the one graph form that makes `supervisor_graph!` emit `unsafe`
code into the consuming crate (the local slot type's `unsafe impl Sync`, whose
soundness contract — every `provide`/`take`/`restore` of a slot on ONE core — the
application owns), so it is now an explicit per-application opt-in. Graphs not using
`local` are unaffected; `local` users add the feature and change nothing else.

### Added
- The `local-resources` feature (non-default): permits the `local` resource kind by
  forwarding to the macro crate. Without it, a `local` marker is a compile error
  naming the feature.

## [0.3.3] - 2026-07-09

Ships the `resources:` **kind markers** by updating the macro pin to
`embassy-supervisor-macros = "=0.4.0"`: `consume` (the worker owns the value — drop it
at teardown; the slot stays empty, so a respawn fail-closes until the app re-provides a
fresh instance), `shared` (a fan-out slot for a `Copy` handle — any number of nodes and
whole pools copy the same value out non-destructively; replaces panicking accessor
extras with a gate-awaited `SpawnError::Busy`), and `local` (a graph-site slot without
the `T: Send` bound, for `!Send` driver handles on a single core). Composed
(`local consume`, `shared local`) they cover the whole radio shape: a `!Send` runner
whose `Drop` must release pins/DMA at teardown and that is rebuilt each wake cycle,
plus the `Stack` handle fanned out to every network consumer. With the new
`slot_timeout:` clause, the builder itself becomes a **provider node** — an ordinary
first-in-topo node that `provide()`s at runtime while its consumers' gate waits
rendezvous with the build (the graph-native `hw_init`; see the README's provider-node
recipe). See the macros CHANGELOG and the README's "Resource kinds" section.

### Added
- `ResourceSlot::get` (`T: Copy` only): copy the value out **without emptying the
  slot** — the `shared` kind's fan-out read, also usable by hand.
- `TaskNode::with_slot_timeout` (backs the macro's `slot_timeout:` clause): per-node
  override of the pre-spawn `executor:`-slot and `resources:`-gate wait bound (the
  default is unchanged at 100 ms). Size it to a provider node's async build time.

## [0.3.2] - 2026-07-08

Decouples node-name stamping from the trace recorders, so a graph's node names can
reach external consumers (rtos-trace/SystemView, debuggers) **without** the
supervisor's own `trace` layer or its `_embassy_trace_*` hook symbols. Updates the
macro pin to `embassy-supervisor-macros = "=0.3.1"`.

### Added
- `TaskNode::stamp_name` (feature `metadata-names`): stamps the node name into the
  task's embassy `Metadata` without capturing the task id or engaging the `trace`
  recorders. Called automatically by the generated spawn glue on the name-only path.
- The `metadata-names` feature: pulls only `embassy-executor/metadata-name` (not
  `embassy-executor/trace`), so a binary built with `metadata-names` (and no
  `trace-hooks`) links with zero `_embassy_trace_*` symbols and pairs cleanly with
  embassy's own `rtos-trace` feature — SystemView shows the graph node names at no
  supervisor-recorder cost.

### Changed
- `trace-names` is now `["trace", "metadata-names"]` (previously it enabled
  `embassy-executor/metadata-name` directly). The activation set and effective
  behavior are identical to prior releases; the name stamp is simply now a
  standalone capability that `trace-names` composes.

## [0.3.1] - 2026-07-07

Ships the `supervisor_graph!` `task:` clause by updating the macro pin to
`embassy-supervisor-macros = "=0.3.0"`: declare a **plain async worker fn** —
possibly generic — and the macro stamps its concrete `#[embassy_executor::task]`
shell per node. See the macros CHANGELOG and the README's "`task:` —
generated shells" section.

### Added
- **Safe resource threading** (backs the macro's new `resources:` clause):
  `ResourceSlot<T>` — a one-value handoff cell moving an owned resource from `main`
  into a supervised task, replacing `Peripherals::steal()` inside the task body.
  `main` `provide()`s the value (consuming the `Peripherals` field — the
  compile-time exclusive-ownership guarantee), the generated spawn glue `take()`s it
  before the spawn (empty slot → `SpawnError::Busy` out of `start()`, fail-closed),
  and the generated shell `restore()`s it after the worker returns so a Terminate
  respawn re-takes the *same instance*. Provisioning is runtime-checked; ownership
  is compile-time.
- `ResourceGate` (the slot's type-erased readiness view) and
  `TaskNode::with_resources`: `start` / `start_node` / `respawn_terminate` await a
  node's resource slots being filled (bounded by the same `SLOT_READY_TIMEOUT` as
  executor slots) before spawning — tolerating late provisioning and closing the
  respawn-races-the-restore window on multi-core graphs.

### Changed
- `defmt` dependency requirement stated as `1.1.0` (the version the crate is built
  and tested against) instead of the imprecise `1`.

## [0.3.0] - 2026-07-06

### Added
- Multi-executor graphs: the `executor NAME;` item declares a runtime-filled
  `SpawnerSlot` (a `SendSpawner` — `InterruptExecutor` tiers, the second core, or a
  foreign thread executor via `make_send()`), and `executor: NAME` on a node routes its
  generated spawn through the slot. An unfilled slot fails the spawn with
  `SpawnError::Busy`; annotated nodes' futures must be `Send`.
- `TaskNode::adopt(&SpawnToken)`: one-call registration (task id + `trace-names` name
  stamp) for spawns the macro cannot see (parked nodes, verbatim spawn closures).
- `trace-nested` (opt-in): preemption-exact accounting. A nested higher-tier poll
  credits its wall time back to the window it interrupted, so a preempted node's
  `exec_ticks`/`max_poll_ticks` are no longer inflated and `stalled_task`/watermarks
  name the real culprit. On multi-core systems register `trace::set_core_id_fn`
  (e.g. read `SIO.CPUID` on RP2350) for one preemption stack per core; unregistered,
  everything maps to core 0 (single-core behavior).
- Multi-core support: bring-up awaits a node's `SpawnerSlot` (`ready()`) so the
  supervisor rendezvouses with another core's asynchronous executor bring-up *as part
  of* `start()` (bounded, then `SpawnError::Busy`); pools accept `executor: NAME` too
  (an elastic worker pool on the second core, scaled by this core's supervisor).
  Cross-core lifecycle (slot-routed spawn, shutdown/ack, control) is covered by
  cross-thread host tests running two real executors.
- `TaskNode::with_executor(&SpawnerSlot)`: routes a node's spawn through an executor
  slot (emitted by `supervisor_graph!` for `executor: NAME`).
- `supervisor_graph!` `deps:` may name a `pool` (not only a `node`); it resolves to the
  pool's floor member — "start after the pool is up".
- Trace-hook observability (opt-in features): `trace` — the supervisor consumes
  embassy-executor's `_embassy_trace_*` instrumentation, mapping task ids to nodes via the
  generated spawn glue and accounting per-node poll time / poll count / max-poll watermark,
  per-executor idle time, and live stall detection (`trace::current_task` /
  `trace::stalled_task`), and a per-executor time decomposition (`trace::executor_stats`:
  idle / in-poll / overhead / unsupervised-task share, poll and pass counters);
  `trace-hooks` — `supervisor_graph!` also defines the hook symbols
  at the declaration site; `trace-names` — node names are stamped into task Metadata for
  external consumers. Counters are wrapping u32 ticks (sample-and-diff); accounting is
  preemption-naive and capped at 4 executors (documented).

### Changed
- **Breaking:** `Supervisor::start`, `Supervisor::start_node`, and
  `Supervisor::respawn_terminate` are now `async fn` and must be `.await`ed (they were
  synchronous). Bringing up an `executor: NAME` node now awaits its `SpawnerSlot`
  (bounded by an internal default, then `SpawnError::Busy`) before spawning it, so a
  tier filled late — or from another core — is handled without a race and without the
  hazards of the old synchronous slot wait (no busy-spin; no integrated-timer-queue
  panic on hardware). A node with no `executor:` slot skips the wait. Callers on the
  supervisor task simply add `.await`.

### Fixed
- `pool` without `control` (`default-features = false, features = ["pool"]`) failed to
  compile: the graph-index helpers the pool driver needs lived in a `control`-gated
  impl. They are now gated on either feature.
- Control `Activate` of a detached node no longer re-enables (and potentially
  restarts) the node's dependencies: a detached node's `deps:` are start-ordering
  only, so the activate cascade now skips expanding from a detached member, matching
  the deactivate cascade.

## [0.2.0] - 2026-07-01

The graph moved to compile time, and the `supervisor_graph!` proc-macro shipped in the
new companion crate `embassy-supervisor-macros` (versioned independently and pinned by
exact version; pulled in by the default `macros` feature). **Breaking release** — see
the migration notes in the README.

### Added
- Pool `policy:` accepts an optional explicit type: `policy: <Type> = <expr>`. When the
  type is omitted it is still derived from a `Type::new(..)` value (unchanged); the
  explicit form allows any value of that type (a `const`, a `const fn` factory, a builder
  chain, a qualified path).
- `Debug` impls on `Mode`, `ControlOp`, `ControlCommand`, and `TaskNode` (the latter a
  manual impl printing the name, mode, and live state flags).
- Macro-time validation: graphs are capped at 256 node slots (all graph indices are `u8`;
  a larger graph previously truncated silently), and pool bounds must satisfy
  `min <= max <= member count`.

### Changed
- The graph-declaration macro was renamed from `task_graph!` to `supervisor_graph!`.
- `supervisor_graph!` now emits a single `pub static GRAPH: Graph<M>` bundling the node
  slots, dependency table, topological order, and (with the `pool` feature) the pools,
  replacing the former loose `ALL_NODES` / `DEPS` / `ORDER` / `POOLS` symbols. Read them as
  `GRAPH.nodes` / `GRAPH.deps` / `GRAPH.order` / `GRAPH.pools`.
- `Supervisor::new` takes the bundled graph: `Supervisor::new(&GRAPH)`, replacing the
  previous three-argument `new(&ALL_NODES, &DEPS, ORDER)` form.
- `Supervisor::run_pools` no longer takes a pool-registry argument; it reads the pools from
  the graph (`GRAPH.pools`).

### Removed
- `Supervisor::with_pools` — pools are now part of `GRAPH` and passed via `Supervisor::new`.
- The generated `NODE_COUNT` constant; use `GRAPH.nodes.len()` instead.

### Internal
- Host-runnable unit + integration tests for the dependency-ordered topo sort, cycle
  detection, and the `DeferredShrink`/`ElasticPool` scaling logic, plus a GitHub Actions CI
  workflow (host tests, `thumbv8m` no_std build, clippy, fmt, doc). Test-only dev-dependencies
  are gated to non-embedded targets, so the shipped crate stays `no_std` and driver-agnostic.

## [0.1.1]

### Fixed
- `task_graph!` accepts the final node with or without a trailing comma. The documented
  `task_graph! { &A, &B }` form (no trailing comma) previously hit a macro recursion limit;
  both styles now expand correctly.

## [0.1.0]

Initial release.

- Dependency-ordered task bring-up and reverse-ordered teardown over a `task_graph!` of `TaskNode`s
  (topological sort, no allocation).
- Lifecycle modes: `Terminate`, `Pause`, `OnDemand`.
- Elastic worker pools (`ElasticPool` with a swappable `ScalingPolicy`, e.g. `DeferredShrink`)
  behind the `pool` feature.
- Decoupled runtime start/stop/pause/resume control (`request_control` / `apply_control`) behind the
  `control` feature.
- Optional `defmt` logging behind the `defmt` feature (no-op otherwise).

[0.5.1]: https://github.com/cedrivard/embassy-supervisor/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/cedrivard/embassy-supervisor/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/cedrivard/embassy-supervisor/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/cedrivard/embassy-supervisor/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/cedrivard/embassy-supervisor/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.5...v0.4.0
[0.3.5]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/cedrivard/embassy-supervisor/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/cedrivard/embassy-supervisor/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cedrivard/embassy-supervisor/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/cedrivard/embassy-supervisor/releases/tag/v0.1.1
[0.1.0]: https://github.com/cedrivard/embassy-supervisor/releases/tag/v0.1.0
