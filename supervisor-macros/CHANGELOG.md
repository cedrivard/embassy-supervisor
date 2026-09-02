# Changelog

All notable changes to `embassy-supervisor-macros` are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The crate is versioned
independently of `embassy-supervisor`, which pins it by exact version; see the
supervisor's CHANGELOG for the surrounding API history.

## [0.9.0] - 2026-09-01

Rides `embassy-supervisor-syntax = "=0.3.0"`.

### Added

- Feature `budget`: the `divisible` resource kind. One `pub static NAME:
  Budget<K>` per name, sized to its declaring nodes plus pool members
  (counted syntactically, so a `#[cfg]`'d-out holder still reserves its
  slot); the shell receives a `Claimant` bound to the holder's slot (a pool
  wrapper builds member `I`'s as `base + I`); a claims table per holder wired
  with `.with_claims(..)`; shape bit `CLAIMS`. `provides:` may name a budget.
  Rejected: `divisible` with a type or any other kind marker, more than 256
  slots, `pool_size > 1` beside a `divisible` entry (one claimant slot, which
  the instances would share), the marker without the feature.
- Feature `veto`: the `veto` marker on `writes:` entries. Writers of one gate
  are numbered in item order (a node one slot, a pool one per member, with a
  writes table per member so each carries its own slot), the slot rides in
  the entry's `Coupling`, and one `const _: () = __sv_check_veto(&TARGET, K)`
  per gate makes a non-`VetoGate` target a type error and too few slots a
  const-eval error; the check carries the union of its writers' `#[cfg]`s, so
  a gate that rides a feature with them is not named without it. Rejected:
  `veto` on a `reads:` entry, more than 32 writers, one gate spelled two ways
  across the graph (slots are numbered per spelling), the marker without the
  feature.
- `shared serialized`: a re-declaration whose item routes through a different
  `executor:` than the first holder's is rejected, naming both holders and
  their tiers. Syntactic; `#[cfg]`s are not consulted.

### Changed

- The `#[dataflow]` scanner names the missing feature for an `open` or
  `lease` call (`data-deps`), a `veto` call (`veto`) or a `retire` call
  (`data-deps` + `readiness`) instead of leaving rustc to report the method.
- Per-kind codegen branches on `ResourceKind` instead of marker probes, and
  the shared-slot plan is a graph-wide slot plan carrying `divisible` slot
  counts and the first holder's executor. Emitted code for the three existing
  kinds is unchanged.

## [0.8.0] - 2026-08-27

Rides `embassy-supervisor-syntax = "=0.2.0"`.

### Added

- Every value-level clause is `#[cfg]`-aware — `slot_timeout:`, `ack_timeout:`,
  `beat_timeout:`, `beat_window:`, `ready_on_write`, `disabled`, `discover`,
  and per-entry `provides:`. A gated clause whose feature is present emits its
  builder call under the predicate (a `const`-block rebinding, so the config
  chain stays `const`); with the feature absent, the macro-time "requires
  feature" error defers to a `compile_error!` under the same predicate, firing
  only in builds where the clause exists. A gated `disabled` becomes a
  cfg-block `true`/`false` argument to `TaskNode::new`; a gated `discover`
  rides the adopted tables' per-entry machinery, marker assertions included.
  `shape_bits` stays deliberately conservative: a compiled-out clause may
  still set its bit, an over-approximation that costs a driver-loop arm that
  never fires and can never be wrong.

### Changed

- syn 3; the trybuild fixture resolves `embassy-supervisor` with the full
  forwarded feature superset, so the ui suite also runs under
  `--all-features`.

## [0.7.0] - 2026-08-25

### Added

- `ack_timeout: MS` (node and pool) emits `.with_ack_timeout(Duration::from_millis(N))`
  on the node config (every member's, for a pool): the per-node override of the 2 s
  shutdown-ack window.
- `state: zeroed Type`: emits a second helper, `__sv_try_box_zeroed`, that allocates
  with `alloc_zeroed` under a `T: embassy_supervisor::Zeroable` bound; the init-form
  helper is emitted only where an `= init_expr` clause exists.
- `#[dataflow]` carries statement-level `#[cfg]` into the derived tables: a
  verb call under a feature-gated statement emits an entry gated the same way
  (per-entry cfg on the table element, cfg-aware table length, and call sites
  indexing by the cfg-aware count of live entries ahead). A signal reached
  both gated and ungated stays one unconditional entry; distinct gates
  disjoin into `any(..)`.
- `dataflow:` adoption entries take a per-entry `#[cfg(..)]`, like `deps:` and
  `reads:` entries — a feature-gated accessor is ordinary. The bound table set,
  its cfg-aware length, and the `discover` marker assertions all follow the
  build.
- `deps:` is optional, on nodes and pools alike: omitting it is `deps: []`, the
  natural spelling for a graph whose ordering lives in the runtime coupling. It
  is also order-free among the clauses now rather than positional; a duplicate
  clause is a spanned error.
- `provides: [SLOT, ..]` node clause (no feature — the clause is its own opt-in,
  like `slot_timeout:`): names the resource slots
  this node's task fills at runtime, resolved against the graph's `resources:`
  entries (an unknown name is a span-attached expansion error, not rustc's
  "cannot find value"). Emits a per-node `&dyn ResourceGate` array and the
  `.with_provides` builder call, through which the node's shutdown ack clears the
  slots. Duplicate names and an empty list are rejected at parse.
- `beat_timeout: MS` / `beat_window: N` node clauses (feature `liveness-monitor`),
  emitting `.with_beat_timeout(..)` / `.with_beat_window(..)`. Rejected with
  span-attached errors: `beat_timeout: 0`, a `beat_window:` without a `beat_timeout:`,
  and a `beat_window:` outside `1..=255` — the builder takes a `u8` and the sweep reads
  the window as `max(1)`, so out of range would be a type mismatch inside the expansion
  and zero a silent coercion.
- `reads: [path, ..]` / `writes: [path, ..]` node clauses (feature `coupling`). Entries
  are **paths, not expressions**: each is emitted as `&PATH` into a `[Coupling; N]`
  table alongside the path rendered verbatim for diagnostics, so the compiler checks the
  item exists and is `Sync`. An empty list and a repeated path are expansion-time
  errors (duplicates compared as text — one-sided by design: it never rejects a
  correct declaration, and cross-node matching is by address at runtime).
- `bound` dep marker (feature `bound-deps`), emitting the per-node
  `with_bound_deps` array.
- Dep markers now compose in any order (`X ready bound` == `X bound ready`), each at
  most once. `bound` without `ready` is rejected.
- `observed` entry marker on `reads:`/`writes:` (feature `coupling-observe`), emitting
  `.observed(Observer::new(|| ..))`. The accessor comes from a graph-level
  `observe writes: <expr>;` / `observe reads: <expr>;` default with `it` bound to the
  signal, or from a per-entry `observed via <expr>` override. An `observed` entry with
  neither resolves through the supervisor's `Observable` facade (see Changed).
- `it` is substituted into the accessor **at the token level**, rewriting the author's
  own token rather than binding one the macro invents. A graph reaches this macro
  through `macro_rules!` relays (`supervisor_fragment!`, `compose_graph!`), where an
  invented `it` would land in a different hygiene context than the author's and the two
  would never resolve to each other. The substituted signal is parenthesised, so
  `it.load()` cannot reassociate into `&(PATH.load())`.
- Per-entry `#[cfg(..)]` on `reads:`/`writes:` entries, matching `deps:`; the emitted
  table's length is sized with the same `cfg_aware_len` machinery as the dep-marker
  overlays.
- `[idx]` on a signal entry (`reads: [crate::ARR[0]]`), selecting one element of an
  array of signals. Declaring one path both bare and indexed is rejected: `&ARR` and
  `&ARR[0]` are the same address, so nothing downstream could tell the element from
  the whole.
- `open` joins `get`/`reader` as a scanned read verb (feature `dataflow`), so a
  gated access lands in the derived `reads:` table like any other read.
- `#[dataflow]` attribute (feature `dataflow`): derives a task fn's coupling
  tables from the verb calls in its body (receiver-keyed on the fn's `TaskNode`
  parameter), emits them as flash `static`s beside the fn, and rewrites each call
  site to carry its table entry. The arguments register the consumer's own verbs
  (`#[dataflow(read(subscribe), write(publish))]`) and carry nothing else: the idents
  join the table the walk keys on, additively and per fn, with the direction stated
  because the scan is token-level. Span-attached errors for a computed argument, a
  missing `TaskNode` parameter, a registration naming a built-in verb or repeating
  itself, and arguments that are not registrations.
- `discover` clause on nodes and pools (feature `dataflow`): binds the
  `task:`/`spawn:` fn's `#[dataflow]` tables in place of declared lists.
  Rejected on a parked node or on a closure spawn. A `reads:`/`writes:` list
  beside it may only add markers (`observed`, `beat`) to a signal the scan
  already found, checked by an emitted const assertion over the bound tables;
  an unmarked entry is a spanned error.
- `dataflow: [path::to::fn, ..]` clause on nodes and pools (feature
  `dataflow`): adopts the named `#[dataflow]` fns' tables beside the item's
  own sources. Emission builds one table-of-tables per direction (declared list
  first, then `discover`'s, then each adopted fn's) bound via `.with_reads`/
  `.with_writes`.
- `beat` qualifier on an `observed` **write** entry, emitting `.beat()`. Only
  `observed beat` entries feed the sweep-driven heartbeat; an `observed` entry
  without it states a coupling and nothing more. `beat` on a `reads:` entry, or
  without `observed`, is a spanned error — a body-side heartbeat is written at
  the site that produces it (`beat_put`/`beat_writer`, or a `node.beat()` call).
- `ready_on_write` node clause (features `coupling-observe` + `readiness`),
  emitting `.with_ready_on_write()`. Requires an `observed beat` entry in
  `writes:` and `beat_timeout:` — the monitor sweep's poll of that write is what
  asserts the readiness; a body that beats through its verbs asserts its own
  readiness, with `set_ready()` at the write. Missing either requirement is a
  span-attached error.

### Changed

- A per-node `task:` shell calls `mark_exited()` through the node's static
  instead of its `__node` parameter, so the parameter's last use is the worker
  call and it stays out of the task arena (it was live across the worker await
  only for that trailing call). Saves up to 8 B of RAM per node arena where
  the reference crossed an alignment boundary. Pool shells are shared by their
  members and keep the parameter.
- **Breaking (emitted code):** `supervisor_graph!` computes the graph's
  structural `shape` bits at expansion (any `ready`/`bound` marker, `executor:`,
  `resources:`, `Pause`/`OnDemand` mode, `beat_timeout:`, `observed` entry,
  `pool` — `#[cfg]`-gated items counted, conservatively) and emits the graph as
  `Graph<N, GRAPH_TOPOLOGY>`, where the new `GRAPH_TOPOLOGY` alias (named-graph
  form: `<NAME>_TOPOLOGY`) names `Ordered<N, SHAPE>` — or `Flat<SHAPE>` for a
  graph with no `deps:` edge anywhere, in which case the `DEPS` backing table is
  not emitted at all. The alias is what a `static` supervisor annotation names.
- **Breaking (emitted code):** each node's immutable config is emitted as its own
  flash-resident `static` (`__SV_CFG_<NODE>`, built with `NodeCfg::new` + the
  `with_*` chain; one `[NodeCfg; K]` array per pool), and the node static becomes
  `TaskNode::new(&__SV_CFG_<NODE>, disabled)` — so a node's RAM footprint is its
  handle plus one pointer, not its whole declaration.
- The graph grammar now lives in **`embassy-supervisor-syntax`**, a new dependency
  pinned by exact version. A `proc-macro` crate can export nothing but its macros, so
  the parser was unreachable to anything else; the tooling that reads a declaration
  from source can now share it instead of keeping a second definition of the syntax.
  Nothing about the accepted grammar, the diagnostics or the generated code changes.
- Feature gating is a pass over the parsed graph rather than part of parsing. The
  syntax crate accepts every construct and keeps the marker or clause keyword each
  rejection points at; this crate decides which are permitted. Same messages, same
  spans. One consequence: a graph with both a syntax error and a feature error now
  reports the syntax error first.
- The unknown-dep-marker error now lists every valid marker
  (`expected `,`, `]`, or a dep marker (`ready`, `bound`)`), so its `.stderr`
  snapshot changed.
- A bare `observed` with no `via` and no graph-level default resolves through the
  supervisor's `Observable` facade (`embassy-supervisor-observe`) instead of being a
  "needs an accessor" expansion error; a type implementing nothing now gets the
  compiler's trait-bound error at the graph site, which is the diagnostic the
  `observed_without_accessor` fixture snapshots.
- Fragments take plain `crate::…` paths: the macro normalizes them to `$crate`,
  the spelling that survives the relay to a foreign compose site still meaning
  the fragment's crate. At the definition site the two can only mean the same
  thing, so fragment authors write ordinary Rust and stay portable
  (`$crate::…` remains accepted).
- `observe` is a graph-level item accepted **anywhere** in the item list, deliberately:
  `compose_graph!` splices fragment items in front of the graph block, so requiring it
  first would make the defaults unusable from a composed graph. The graph-item error
  now reads `expected `node`, `pool`, `executor`, or `observe``, and the node clause
  list names `ready_on_write`.

### Fixed

- `exit:` on a node declared at a `compose_graph!` site alongside
  `supervisor_fragment!` fragments failed to compile: the shell's `let __out` binding
  was emitted with the macro's call-site span while the exit slot's `provide(__out)`
  used the `exit:` type's span, giving the two occurrences different hygiene contexts.
  Both now use the `exit:` type's span — which fixes the resolution and keeps the
  clause's `unreachable_code` diagnostic pointing at the clause rather than at the whole
  graph declaration.

## [0.6.2] - 2026-08-04

### Fixed
- A `cancel` node's generated shell reserved its worker's state machine **twice**
  in static task storage. The shell drove the worker as
  `node.run_cancellable(worker(..)).await`, passing the future by value into an
  `async fn`, which rustc lays out both as that function's argument and inside the
  select it lives across ([rust-lang/rust#62958]) — so every `cancel` node in a
  graph cost an extra copy of its worker future in `.bss`. The shell now pins the
  worker into its own frame and hands `run_cancellable` a `Pin<&mut _>`, which
  stores it once whatever the callee does with its arguments. Applies to `task:`
  pools too, per member. Drop order is unchanged: the worker is still dropped
  before the shell's resource restores, state drop and exit record.

### Changed
- **MSRV raised to 1.88** (from 1.85), following the workspace; see the
  supervisor's CHANGELOG. Nothing in the generated code requires it.

## [0.6.1] - 2026-08-03

### Fixed
- `cancel` on a `task:` declared with the **partial-call form** and no
  `resources:`/`state:` failed to compile: with the node lead suppressed and
  nothing to take its place, the injected argument list emitted a leading comma
  (`entry(, "arg")`), which rustc reported as `expected expression, found ','`
  spanned on the whole `supervisor_graph!` item. The lead and the user's extras
  are now one list.

## [0.6.0] - 2026-08-03

Pairs with `embassy-supervisor` 0.4.1 (macro pin only; no runtime change).

### Added
- `cancel` flag on `task:` nodes: the generated shell drives the worker under
  `TaskNode::run_cancellable` and does NOT inject the node, so a plain
  supervisor-unaware `async fn` (even diverging) binds directly — `resources:`
  become the worker's first arguments. On stop/teardown the worker's future is
  dropped in place and the shell still runs its full tail (state drop, resource
  restores, exit record, ack), so a `Terminate` respawn re-takes the same
  instances. With `exit:` the value is provided only on a real completion; an
  aborted worker leaves the exit slot empty, which is how a waiter tells
  "finished" from "stopped". The trade is that the worker gets no post-cancel
  code: a task that must flush or release something *ordered* at teardown keeps
  the node argument and races `run_cancellable_acked` itself.
- `cancel` on `task:` pools, as the trailing clause (`min: N, max: M[,
  slot_timeout: MS], cancel;`). It applies to the one shell all members share, so
  an elastic shrink can retire a member that would never have acked: its future
  is dropped in place and its per-member resources are restored to its own slot
  index, ready for the regrow. A `cancel` member holds no node, so its busy/idle
  load signal has to be driven from the app on the member static.
- Two spanned rejections for the flag, on nodes and pools alike: `cancel` without
  `task:` (it rewrites how the *generated* shell calls the worker; a `spawn:` fn
  can call `node.run_cancellable(..)` itself), and `cancel` with `Pause` (the
  node mode or any pool member: a Pause worker must survive the stop and park on
  `wait_resume()`, but `cancel` drops its future and records an exit).

### Changed
- `exit:` on a worker that can never return (`-> !`) is now a **compile error**
  instead of a silently dead provide: the slot could never be filled and a
  `wait_take()` on it would hang forever. The generated provide re-denies
  `unreachable_code` on itself, so rustc reports `unreachable statement` (or
  `unreachable call` under `cancel`) spanned on the `exit:` clause. Diverging
  workers without `exit:` are unaffected — `cancel`, `Mode::Pause` and detached
  daemons all rely on them.

## [0.5.0] - 2026-08-02

Pairs with `embassy-supervisor` 0.4.0 (node-completion observation, exit values).

### Added
- `ready` dep marker (feature `readiness`, forwarded from the supervisor):
  `deps: [NET ready, WATCHDOG]` emits a per-item ready-dep array wired via
  `.with_ready_deps` — bring-up awaits each marked dep's `set_ready()`. A marker
  naming a pool resolves to the floor member; markers on a `pool`'s `deps:` apply
  to every member. Without the feature the marker is a span-attached error.
- `exit: Type` node clause (`task:` only): emits
  `pub static <NODE>_EXIT: ResourceSlot<Type>` and the shell binds the worker's
  return value and `provide()`s it just before `mark_exited()`. Rejected with a
  targeted error on `spawn:` nodes and on `pool` (K members share one shell).
- `name: IDENT;` graph header: renames the emitted graph static (default
  `GRAPH`) and suffixes the private tables and per-graph helpers
  (`__SvLocalResourceSlot`, the heap-state boxing fn, the alloc alias) so
  several graphs coexist, even in one module. Only the UNNAMED graph emits the
  `trace-hooks` symbols (once-per-binary `no_mangle`).
- `state: Type = init_expr` clause (feature `heap-state`; `task:` nodes and pool
  members): the glue fallibly boxes the init BEFORE the resource takes (a failed
  alloc strands nothing), the shell lends `&mut Type` and drops the Box first
  thing after the worker returns. Emits the consumer-crate `__sv_try_box`
  helper once per graph. Rejected without the feature, and on `spawn:` items.
- `supervisor_fragment!` (new proc macro) + the `@fragment`/`@endfragment`
  attribution markers `supervisor_graph!` now accepts: fragments forward their
  items through a `#[macro_export]` relay into one compose-site expansion (the
  supervisor crate's `compose_graph!` drives the chain). Fragment syntax is
  validated at the fragment site; only `$crate` is permitted as a `$` token.
- Pools accept take-kind `resources:` (per-member `[ResourceSlot<T>; K]` arrays,
  member `I` takes/restores element `I`; shell restores through a slot-reference
  parameter so the index cannot drift); `shared` entries stay one pool-wide
  fan-out slot — including `shared local`, as in 0.4.x — while take-kind
  `local` on pools remains rejected (the single-core slot contract +
  per-member restore is deferred).
- Pool `min:`/`max:` parse as expressions: literals validate at expansion,
  otherwise the emitted consts + const asserts carry the checks.

### Changed
- The generated `task:` shell now calls `__node.mark_exited()` after the worker
  returns and resources are restored, so a worker that exits on its own is recorded
  as completed instead of reading as running forever. The
  `#[allow(unreachable_code)]` on the shell body is now emitted unconditionally
  (the completion record is an unconditional trailing statement; a `-> !` worker
  makes it unreachable, which stays legitimate).

### Fixed
- Add missing doc strings.

## [0.4.1] - 2026-07-09

Safety fix to 0.4.0: the `local` resource kind is the one graph form that makes
`supervisor_graph!` emit `unsafe` code (the local slot type's `unsafe impl Sync`) into
the CONSUMER'S crate — injecting unsafe code must be an explicit opt-in.

### Changed
- The `local` kind marker now requires the new **`local-resources`** feature
  (non-default; forwarded from the supervisor crate's feature of the same name).
  Without it, a `local` marker is a span-attached compile error naming the feature.
  No other behavior changes; graphs not using `local` are unaffected.

## [0.4.0] - 2026-07-09

Requires `embassy-supervisor` >= 0.3.3 (the generated `local` slot type names its
`_export` shim); pinned by exact version from the supervisor crate (`=0.4.0` as of
supervisor 0.3.3).

### Added
- `resources:` **kind markers** — per-entry, order-free, composable:
  `resources: [NAME: [local] [shared|consume] Type, ..]`.
  - `consume`: the worker receives the value **by value** and the shell emits no
    restore — the slot stays empty after the task exits, so the worker may *drop* the
    resource at teardown (a driver whose `Drop` releases pins/DMA) and a respawn
    fail-closes with `SpawnError::Busy` until the application `provide()`s a fresh
    value (the pattern for resources rebuilt each wake cycle).
  - `local`: the entry's slot is a graph-site type (`__SvLocalResourceSlot`) with the
    `ResourceSlot` protocol but no `T: Send` bound, for `!Send` driver handles
    (`RefCell`-/`NoopRawMutex`-based) on a single core. Emitted at the declaration
    site because it carries an `unsafe impl Sync`; its soundness contract is
    single-core use, and a consumer crate forbidding `unsafe_code` cannot use it.
  - `shared`: a fan-out slot for a `Copy` handle — the glue copies the value out
    non-destructively (`get()`, whose `T: Copy` bound enforces the kind), the worker
    receives it by value, no restore, and the slot STAYS FILLED. Any number of nodes
    (and whole `task:` pools — the only `resources:` kind pools accept) may declare
    the SAME slot name: the static is emitted once, gated by the union of the
    declaring sites' `#[cfg]` predicates, and every re-declaration must repeat the
    kinds + type verbatim. Mutually exclusive with `consume`.
  - The markers are contextual keywords: `local`/`consume`/`shared` followed by `::`,
    `<`, or the entry end still parse as (part of) the type.
- `resources:` entries accept per-entry `#[cfg(...)]`: the slot static, gate entry,
  glue take/get, shell parameter, and worker-call argument all follow it (gate the
  worker fn's matching parameter with the same attribute). The node's gate array
  length is cfg-aware.
- The `slot_timeout: MS` clause (nodes and pools; milliseconds ≥ 1) — emits
  `TaskNode::with_slot_timeout`, overriding the 100 ms default bound on the pre-spawn
  `executor:`-slot and `resources:`-gate waits. Sized to a **provider node**'s async
  build time, it turns runtime provisioning into a rendezvous (see the README's
  provider-node recipe).
- New compile errors: a repeated kind marker on one entry; `shared` combined with
  `consume`; a `shared` slot re-declared with different kinds/type; a non-`shared`
  resource on a `pool` (previously all pool `resources:` were rejected); pool
  `resources:` without `task:`; `slot_timeout: 0`; and `local` combined with
  `executor:` on a node or a pool (a `SpawnerSlot`-routed spawn needs a `Send`
  future).
- Generated shells with restore statements carry `#[allow(unreachable_code)]`, so a
  diverging (`-> !`) worker with restore-kind resources no longer warns on the
  (legitimately) unreachable restores.

## [0.3.1] - 2026-07-08

Requires `embassy-supervisor` >= 0.3.2; pinned by exact version from the supervisor
crate (`=0.3.1` as of supervisor 0.3.2).

### Added
- The `metadata-names` feature: a name-only spawn path. When it is on but `trace` is
  off, `spawn_stmts` binds the `SpawnToken` and calls `TaskNode::stamp_name(&token)`
  (node name → task `Metadata`) instead of `adopt` — no task-id capture and no
  dependency on the `_embassy_trace_*` hooks, so a graph gets its node names into
  external consumers (rtos-trace/SystemView) without pulling in the trace recorders.

### Changed
- `trace-names` is redefined as `["trace", "metadata-names"]` (was `["trace"]`). Same
  effective codegen when `trace` is on (the `adopt` path, which stamps the name under
  `metadata-names`); the split just lets the name stamp be requested on its own.

## [0.3.0] - 2026-07-07

Requires `embassy-supervisor` >= 0.3.0; pinned by exact version from the supervisor
crate (`=0.3.0` as of supervisor 0.3.1).

### Added
- The `task:` node/pool clause: name a **plain async worker fn** — possibly generic —
  and the macro stamps the concrete `#[embassy_executor::task]` shell per declaration
  (embassy forbids generic tasks: one static `TaskPool` per concrete future type).
  Same path/partial-call forms as `spawn:`; worker args are evaluated inside the shell
  at the task's first poll, on the node's own executor; trace adoption and `executor:`
  routing compose unchanged.
- `pool_size: N` on a `task:` node sizes the generated shell's `TaskPool` (default 1);
  a `task:` pool emits one shell sized to the member count.
- The `resources: [NAME: Type, ..]` node clause (requires `task:`) — **safe resource
  threading**: each entry emits a `pub static NAME: ResourceSlot<Type>` at the
  declaration site. `main` moves the resource in with `NAME.provide(..)` (consuming
  the `Peripherals` field — compile-time exclusive ownership, no `steal()` inside the
  task), the generated glue `take()`s it just before the spawn (an unprovided slot
  fails `Supervisor::start` with `SpawnError::Busy`, not a task-side panic), and the
  shell hands the worker `&mut Type` (after the node arg, in declared order, before
  partial-call extras) and `restore()`s the value after the worker returns — a
  Terminate respawn re-takes the *same instance*. The node is emitted with
  `.with_resources(..)` so the supervisor awaits provisioning/restore before each
  (re)spawn.
- Each `pool` also emits the structural `pub const`s `<POOL>_MIN`, `<POOL>_MAX`, and
  `<POOL>_MEMBERS` (`usize`), so downstream compile-time sizing can derive from the
  DSL instead of duplicating it (e.g. `const SOCKET_BUDGET: usize = HTTP_MAX + 1`) —
  a `const` cannot read these off the member `static` array (E0013).
- New compile errors: `task:` combined with `spawn:`, a closure in `task:`,
  `pool_size:` without `task:` (or zero), `resources:` without `task:`, an empty
  `resources:` list, a duplicate resource name (within a node or across the graph),
  and `resources:` on a `pool` (members would contend for a single instance).

## [0.2.0] - 2026-07-06

Requires `embassy-supervisor` >= 0.3.0 (the generated `executor:` glue uses that
release's async slot rendezvous); pinned by exact version from the supervisor crate.

### Added
- The `executor NAME;` item (emits a `pub static NAME: SpawnerSlot`) and the
  `executor: NAME` node clause: the generated glue spawns through the named slot's
  `SendSpawner` instead of the supervisor's `Spawner`. Unknown names, `executor:`
  without `spawn:`, and `executor:` with a verbatim closure are expansion errors.
- Pools accept `executor: NAME` too (between `deps:` and `spawn:`): every member
  spawns through the slot — a worker pool on another executor or core.
- `deps:` may name a `pool` (not just a `node`); the dep resolves to the pool's floor
  member (member 0, the `min`-kept one), i.e. "start after the pool is up". Previously a
  dep on a pool name was an "unknown dependency" error.
- A repeated dependency (`deps: [A, A]`; compared by resolved slot, so a repeated pool
  name counts too) and a redeclared node/pool name are now spanned compile errors.
  Previously a duplicate dep surfaced as a bogus "dependency cycle" and a duplicate
  name silently rewired earlier `deps:` edges before failing downstream.
- Pool `min:`/`max:` emit the validated `u8` values instead of the raw literals, so a
  suffixed literal (`min: 3usize`) no longer produces a mismatched-type rustc error.
- The unknown-dependency error now says "not a declared node or pool".
- An `executor:` node/pool now emits `TaskNode::with_executor(&NAME)`; its spawn glue
  does a non-blocking `SpawnerSlot::get()` because the supervisor awaits the slot
  before invoking it (see the supervisor's 0.3.0 async bring-up).
- Forwarded trace features: under `trace` the generated spawn glue captures each
  `SpawnToken`'s task id into its node (`set_task_id`); under `trace-names` it also stamps
  the node name into the task Metadata; under `trace-hooks` the macro defines the seven
  `_embassy_trace_*` hook symbols at the graph declaration site (the supervisor crate is
  `forbid(unsafe_code)` and cannot; requires an edition-2024 consumer).

## [0.1.0] - 2026-07-01

First published version (previously an unpublished workspace member).

- `supervisor_graph!`: declares `node`/`pool` items once and emits the node `static`s,
  per-pool `ElasticPool` + spawn glue, and a single `pub static GRAPH: Graph<M>` bundling
  the node slots, dependency table, elastic pools, and the topological order computed at
  compile time (a dependency cycle or unknown dependency is a compile error).
- Items and individual deps may carry `#[cfg(...)]`; absent nodes keep their slot as `None`.
- Pool `policy:` accepts an optional explicit type (`policy: <Type> = <expr>`); without it
  the type is derived from a `Type::new(..)`-shaped value.
- Graphs are capped at 256 node slots (indices are `u8`); pool bounds are validated
  (`min <= max <= member count`) at expansion time.
- The `pool` feature (forwarded by `embassy-supervisor`) gates pool emission.

[0.9.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.8.0...embassy-supervisor-macros-v0.9.0
[0.8.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.7.0...embassy-supervisor-macros-v0.8.0
[0.7.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.6.2...embassy-supervisor-macros-v0.7.0
[0.6.2]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.6.1...embassy-supervisor-macros-v0.6.2
[0.6.1]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.6.0...embassy-supervisor-macros-v0.6.1
[0.6.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.5.0...embassy-supervisor-macros-v0.6.0
[0.5.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.4.1...embassy-supervisor-macros-v0.5.0
[0.4.1]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.4.0...embassy-supervisor-macros-v0.4.1
[0.4.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.3.1...embassy-supervisor-macros-v0.4.0
[0.3.1]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.3.0...embassy-supervisor-macros-v0.3.1
[0.3.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.2.0...embassy-supervisor-macros-v0.3.0
[0.2.0]: https://github.com/cedrivard/embassy-supervisor/compare/embassy-supervisor-macros-v0.1.0...embassy-supervisor-macros-v0.2.0
[0.1.0]: https://github.com/cedrivard/embassy-supervisor/releases/tag/embassy-supervisor-macros-v0.1.0
