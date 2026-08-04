//! Proc-macro for `embassy-supervisor`.
//!
//! `supervisor_graph!` is the **single source** of a task graph: it declares the
//! nodes (and an optional elastic pool), generates their `static`s, and computes
//! the topological order at **compile time**. A dependency cycle is a compile
//! error; an unknown dependency name is a compile error.
//!
//! Surface (each item may be `#[cfg(...)]`-prefixed):
//! ```text
//! node NAME = Mode, deps: [A, B], spawn: <spawn>[, executor: EXEC][, disabled];
//! node NAME = Mode, deps: [A, B], task: <worker>[, pool_size: N][, executor: EXEC]
//!     [, resources: [[#[cfg(..)]] RES: [local] [shared|consume] Type, ..]]
//!     [, slot_timeout: MS][, cancel][, disabled];
//! node NAME = Mode, deps: [A];                 // neither => a parked node the app spawns
//! executor EXEC;                               // runtime-filled SendSpawner slot
//! pool NAME = [Mode, ..], deps: [A][, executor: EXEC], spawn: <fn> | task: <worker>,
//!     [resources: [RES: [local] shared Type, ..],]
//!     policy: [<Ty> =] <expr>, min: N, max: M[, slot_timeout: MS][, cancel];
//! ```
//! `deps:` entries name a `node` or a `pool`; a `pool` dep resolves to that pool's floor
//! member (member 0, the `min`-kept one), i.e. "start after the pool is up". A repeated
//! dep or a redeclared node/pool name is a compile error.
//!
//! An `executor NAME;` slot may carry `#[cfg(...)]`, but validation does not model cfg
//! predicates: a node referencing a slot that is cfg'd *out* while the node is cfg'd
//! *in* surfaces as rustc's `cannot find value NAME`, not a macro error — don't gate an
//! executor slot more restrictively than the nodes that reference it.
//! `executor EXEC;` emits a `pub static EXEC: SpawnerSlot`; the app fills it with a
//! `SendSpawner` (`InterruptExecutor::start`, `Spawner::make_send`) before
//! `Supervisor::start`, and nodes carrying `executor: EXEC` spawn through it instead
//! of the supervisor's own executor (their futures must be `Send`; an unfilled slot
//! fails the spawn with `SpawnError::Busy`).
//! A pool is emitted as `ElasticPool<P>`, so the macro needs the policy type `P`. By
//! default it derives `P` from a `Ty::new(..)`-shaped `policy:` value (e.g.
//! `DeferredShrink::new(..)` => `P = DeferredShrink`). Give `policy: <Ty> = <expr>` to
//! state `P` explicitly when the value isn't that shape — a const, a free fn, a builder
//! chain (`X::new(..).with(..)`), or a qualified path.
//! `spawn:` takes a path or a partial call to a task fn taking the node **first**
//! (`spawn: f` => `s.spawn(f(&NAME)?)`; `spawn: f(a)` => `s.spawn(f(&NAME, a)?)`), or,
//! for a node, a closure / ready spawn fn emitted verbatim (for anything that doesn't
//! fit that shape). A pool's `spawn:` is the same path/partial-call form with `&POOL[j]`
//! injected first, via a generated `spawn_<pool>::<j>` glue fn; a pool has no closure
//! form (members are instantiated per index).
//!
//! `task:` takes the same path/partial-call forms but names a **plain async worker
//! fn** — possibly generic (turbofish or inferred) — instead of a hand-written
//! `#[embassy_executor::task]`. The macro stamps a concrete shell task per
//! declaration (embassy forbids generic tasks: one static `TaskPool` per concrete
//! future type), sized by `pool_size:` on a node (default 1) or by the member count
//! on a pool. Worker args are evaluated **inside the shell** — at the task's first
//! poll, on the node's own executor — so cross-node data should go through awaited
//! accessors, and a cross-core node builds its resources on its own core. `task:`
//! and `spawn:` are mutually exclusive; `pool_size:` requires `task:`.
//!
//! `cancel` (a bare flag on `task:` items) makes the shell own the shutdown
//! race: the worker is driven under `TaskNode::run_cancellable` and does NOT
//! receive the node (resources become the first arguments), so a plain
//! supervisor-unaware `async fn` — even a diverging one — binds directly. On
//! stop/teardown its future is dropped in place and the shell still runs its
//! full tail (state drop, resource restores, exit record). With `exit:` the
//! value is provided only on a real completion — an aborted worker leaves the
//! exit slot empty. Rejected on `spawn:` (that fn owns its body) and on
//! `Mode::Pause` (a Pause worker must survive the stop and park on
//! `wait_resume()`; `cancel` would record an exit nothing resumes). On a `pool`
//! it is the trailing flag (after `max:`/`slot_timeout:`) and applies to the one
//! shared shell, i.e. to every member: a shrink drops that member's future in
//! place and its per-member resources are restored to its own slot index.
//!
//! **Prefer `task:`** — no attribute boilerplate, generic workers, auto-sized pool
//! shells, and it is the only form supporting `resources:`; the shell inlines into
//! the same poll and its `TaskPool` replaces the one the attribute would emit.
//! `spawn:` remains for: a fn that already carries `#[embassy_executor::task]` and
//! can't be de-attributed (another crate); a task also spawned outside the graph
//! (sharing its one `TaskPool` instead of duplicating it as a shell); the verbatim
//! closure form (custom spawn-time logic); and args that must be evaluated at
//! spawn time on the supervisor's executor rather than at the shell's first poll.
//! Worked examples: README "`spawn:` vs `task:` — which to use".
//!
//! Two `task:` footguns, spelled out because nothing warns about either:
//!
//! * a partial-call **extra that can be missing at first poll is a task-side
//!   panic**, not a failed spawn — extras are for infallible accessors. A value
//!   that might not exist yet belongs in `resources:` (a `shared` entry for a
//!   fan-out handle), where the pre-spawn gate turns "missing" into a clean
//!   `SpawnError::Busy`;
//! * a **verbatim-closure `spawn:` node is invisible to the trace/name glue** —
//!   the closure owns the `SpawnToken`, so `adopt`/`stamp_name` is YOUR job
//!   inside it, and a stable proc-macro cannot emit a warning when you forget.
//!
//! `resources: [RES: [local] [shared|consume] Type, ..]` (requires `task:`)
//! threads **owned resources from `main`** into the worker instead of re-acquiring
//! them inside the task (`Peripherals::steal()`). Each entry emits a
//! `pub static RES` slot at the declaration site; `main` moves the
//! resource in with `RES.provide(..)` (consuming the `Peripherals` field — the
//! compile-time exclusive-ownership guarantee), the generated glue `take()`s it just
//! before the spawn (an unprovided slot fails `Supervisor::start` with
//! `SpawnError::Busy` after a bounded wait — fail-closed, not a task-side panic),
//! and the shell passes the worker `&mut Type` (after the node arg, in declared
//! order, before any partial-call extras) and `restore()`s the value after the
//! worker returns, so a Terminate respawn re-takes the *same instance*. Take-kind
//! slot names are statics: unique across the graph. Entries may carry per-entry
//! `#[cfg(...)]` (the slot, gate, glue, shell param, and worker-call argument all
//! follow it — gate the worker fn's matching parameter with the same `#[cfg]`).
//!
//! Per-entry kind markers refine that default (order-free; `local` composes with
//! either of the mutually-exclusive `consume`/`shared`):
//!
//! * `consume` — the worker receives the value **by value** and no restore is
//!   emitted: the slot stays empty after the task exits, so the worker may *drop*
//!   the resource at teardown (a driver whose `Drop` releases pins/DMA) and a
//!   respawn fail-closes (`SpawnError::Busy`) until the application `provide()`s
//!   a fresh value — the pattern for resources rebuilt each run (e.g. radio
//!   driver objects that go stale across a power cycle).
//! * `shared` — a fan-out slot for a `Copy` handle (an `embassy_net::Stack`, a
//!   `&'static` shared-bus ref): the glue copies the value out non-destructively
//!   (`get()` — `T: Copy` enforced by its bound), the worker receives it by
//!   value, no restore, and the slot STAYS FILLED — so any number of nodes
//!   (and whole `task:` pools, where it stays ONE pool-wide slot — take kinds
//!   become per-member arrays there instead) may declare the SAME slot name.
//!   The static is emitted once, with the union of the declaring sites' cfg
//!   predicates; every re-declaration must repeat the kinds + type verbatim.
//! * `local` — **requires the non-default `local-resources` feature** (of the
//!   supervisor crate, forwarded here): the slot is the graph-site
//!   `__SvLocalResourceSlot` type instead of `ResourceSlot` — same protocol, no
//!   `T: Send` bound, for `!Send` driver handles
//!   (`RefCell`-/`NoopRawMutex`-based). Feature-gated because it is the one
//!   graph form that emits `unsafe` code into the CONSUMER'S crate: an
//!   `unsafe impl Sync` whose soundness is the **single-core contract** (all
//!   `provide`/`take`/`restore` of the slot on one core). It cannot combine
//!   with `executor:` (a `SendSpawner`-routed node needs a `Send` future —
//!   macro error), and a consumer crate forbidding `unsafe_code` cannot use
//!   `local` (the assertion lands in *its* code, like the `trace-hooks`
//!   symbols).
//!
//! `slot_timeout: MS` (node and pool; milliseconds ≥ 1) overrides the node's
//! pre-spawn wait bound for its `executor:` slot and `resources:` gates (default
//! 100 ms — sized for "provided before `start()`"). Raise it for consumers of a
//! **provider node** — a first-in-topo node whose worker *builds* the resources
//! at runtime and `provide()`s them (the graph-native `hw_init`): size the
//! timeout to the provider's async build time and the gate wait becomes a
//! rendezvous instead of a `Busy`. See the README's provider-node recipe.
//!
//! A graph holds at most **256 node slots** (including pool members): all graph
//! indices are `u8`, and the macro rejects a larger declaration at expansion.
//!
//! Nodes, pools, and individual deps may carry `#[cfg(...)]` attributes. A
//! proc-macro can't evaluate `cfg`, so the node array is a fixed-length
//! `[Option<&TaskNode>; M]` over all declared slots (each entry `Some`/`None` via a
//! cfg-expression), the dep table is cfg-aware per-dep, and the order runs through
//! `topo_sort_const` at const-eval (after cfg). Absent nodes are skipped at runtime.
//!
//! Generated items (at the call site): one `pub static` per `node`, a `[TaskNode; K]`
//! array + `spawn_<pool>` glue fn + `<POOL>_POOL` `ElasticPool` + the structural
//! `pub const`s `<POOL>_MIN` / `<POOL>_MAX` / `<POOL>_MEMBERS` (usize; for
//! const-context sizing downstream — a `const` cannot read them off the member
//! `static`) per `pool`, one slot `pub static` per `resources:` entry (shared
//! entries: one per unique name; plus, iff any entry is `local`, the
//! `__SvLocalResourceSlot` type), plus a
//! single `pub static GRAPH: Graph<M>` bundling the node slots, the dependency table,
//! the topological order, and (with the `pool` feature) the pools — pass `&GRAPH` to
//! `Supervisor::new`. The backing tables are private; read them through `GRAPH.nodes`
//! / `GRAPH.deps` / `GRAPH.order` / `GRAPH.pools` (node count is `GRAPH.nodes.len()`).
//!
//! With the supervisor's `trace` feature (forwarded here) the generated spawn glue
//! also captures each `SpawnToken`'s task id into its node (`set_task_id`); with
//! `metadata-names` it stamps the node name into the task Metadata. These are
//! independent: `metadata-names` without `trace` emits a name-only spawn path
//! (`stamp_name`, no id capture, no `_embassy_trace_*` dependency), so node names
//! reach external tooling (rtos-trace/SystemView) without the trace recorders.
//! With `trace-hooks` the macro additionally defines the seven `_embassy_trace_*`
//! hook symbols at the declaration site (the supervisor crate is
//! `forbid(unsafe_code)` and cannot), forwarding to the supervisor's `trace`
//! recorders — requires an edition-2024 consumer, and exactly one graph declaration
//! (or hook set) per binary.
//!
//! Types are referenced absolutely (`::embassy_supervisor::…`), so the consuming
//! crate must depend on `embassy-supervisor` under its real name (not aliased).

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::{HashMap, HashSet};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    Attribute, Expr, Ident, LitInt, Meta, Path, Result as SynResult, Token, Type, bracketed,
};

mod kw {
    syn::custom_keyword!(node);
    syn::custom_keyword!(pool);
    syn::custom_keyword!(deps);
    syn::custom_keyword!(spawn);
    syn::custom_keyword!(task);
    syn::custom_keyword!(pool_size);
    syn::custom_keyword!(policy);
    syn::custom_keyword!(min);
    syn::custom_keyword!(max);
    syn::custom_keyword!(disabled);
    syn::custom_keyword!(executor);
    syn::custom_keyword!(resources);
    syn::custom_keyword!(slot_timeout);
    syn::custom_keyword!(exit);
    syn::custom_keyword!(name);
    syn::custom_keyword!(state);
    syn::custom_keyword!(cancel);
    syn::custom_keyword!(fragment);
    syn::custom_keyword!(endfragment);
}

/// The graph-site slot type emitted (once per graph, iff any `resources:` entry
/// is `local`-marked) for `!Send` resources. A single shared name: `emit_node`
/// types the slot statics with it and `expand` emits its definition. Like the
/// fixed `GRAPH` static, at most one `supervisor_graph!` per module.
const LOCAL_SLOT_TYPE: &str = "__SvLocalResourceSlot";

/// Per-graph idents for the generated helper items. UNNAMED graphs keep the
/// historical fixed names; a `name: X;` graph suffixes them so several graphs
/// coexist, even in one module.
struct HelperIdents {
    /// The graph-site `!Send` slot type (`local` kind).
    local_slot: Ident,
    /// The fallible-boxing fn (`state:` clauses).
    try_box: Ident,
    /// The `extern crate alloc as …` alias the try-box helper and shells use.
    alloc_alias: Ident,
}

impl HelperIdents {
    fn new(graph_name: Option<&Ident>) -> Self {
        match graph_name {
            None => Self {
                local_slot: format_ident!("{LOCAL_SLOT_TYPE}"),
                try_box: format_ident!("__sv_try_box"),
                alloc_alias: format_ident!("__sv_alloc"),
            },
            Some(n) => {
                let lower = n.to_string().to_lowercase();
                Self {
                    local_slot: format_ident!("{LOCAL_SLOT_TYPE}{}", n),
                    try_box: format_ident!("__sv_try_box_{lower}"),
                    alloc_alias: format_ident!("__sv_alloc_{lower}"),
                }
            }
        }
    }
}

/// A dependency reference: a node ident, optionally `#[cfg(...)]`-gated.
#[derive(Clone)]
struct Dep {
    cfg: Vec<Attribute>,
    ident: Ident,
    /// `deps: [NET ready]` — bring-up additionally awaits the dep's
    /// task-asserted readiness (`set_ready`), bounded by the dependent's
    /// `slot_timeout`. The `Ident` is kept for its span (feature errors).
    ready: Option<Ident>,
}

/// `deps: [a, #[cfg(feature = "x")] b, c ready, …]`
fn parse_dep_list(input: ParseStream) -> SynResult<Vec<Dep>> {
    let content;
    bracketed!(content in input);
    let mut deps = Vec::new();
    while !content.is_empty() {
        let cfg = content.call(Attribute::parse_outer)?;
        let ident: Ident = content.parse()?;
        // Optional contextual `ready` marker: a dep entry is otherwise a lone
        // ident, so a following ident can only be the marker.
        let ready = if content.peek(Ident) {
            let marker: Ident = content.parse()?;
            if marker != "ready" {
                return Err(syn::Error::new_spanned(
                    &marker,
                    format!("expected `,`, `]`, or the `ready` marker, found `{marker}`"),
                ));
            }
            if !cfg!(feature = "readiness") {
                return Err(syn::Error::new_spanned(
                    &marker,
                    "the `ready` dep marker requires the `readiness` feature \
                     (embassy-supervisor feature `readiness`) — bring-up then \
                     awaits the dep's set_ready() before spawning this node",
                ));
            }
            Some(marker)
        } else {
            None
        };
        deps.push(Dep { cfg, ident, ready });
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        }
    }
    Ok(deps)
}

/// `[Terminate, OnDemand, …]` — the bracketed mode list.
fn parse_mode_list(input: ParseStream) -> SynResult<Vec<Ident>> {
    let content;
    bracketed!(content in input);
    let punct = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?;
    Ok(punct.into_iter().collect())
}

/// How a node/pool member gets its task: `spawn:` names a hand-written
/// `#[embassy_executor::task]` fn (path / partial call / verbatim closure), while
/// `task:` names a **plain async fn** — possibly generic — for which the macro
/// emits a concrete `#[embassy_executor::task]` shell (embassy forbids generic
/// tasks: one static `TaskPool` per concrete future type, so per-type shells are
/// the only way — the macro stamps them so the user doesn't).
enum TaskSource {
    /// `spawn: <expr>` — the expr *is* (or produces) the task fn.
    Spawn(Expr),
    /// `task: <path | partial call>` — wrap in a generated shell; args are
    /// evaluated inside the shell (at the task's first poll, on its own executor).
    Shell(Expr),
}

/// One `[#[cfg(..)]] NAME: [local] [shared|consume] Type` entry of a
/// `resources:` clause. The macro emits a `pub static NAME` slot at the
/// declaration site (`ResourceSlot<Type>`, or the graph-site local slot type
/// for `local` entries); `main` moves the resource in with `NAME.provide(..)`
/// (consuming the `Peripherals` field — the compile-time ownership guarantee),
/// the generated spawn glue `take()`s (or, for `shared`, copies via `get()`) it
/// before the spawn, and the generated shell `restore()`s it after the worker
/// returns so a respawn re-takes the same instance (unless `consume`/`shared`).
struct ResourceDecl {
    /// Per-entry `#[cfg(...)]` attributes: the slot static, gate entry, glue
    /// take/get, shell param, worker-call argument, and restore all carry them,
    /// so a feature-varying resource set works within one node (the worker fn
    /// must gate its matching parameter with the same `#[cfg]`).
    cfg: Vec<Attribute>,
    ident: Ident,
    ty: Type,
    /// `local` marker: the slot holds a `!Send`-capable value (`Rc`-, `RefCell`-,
    /// `NoopRawMutex`-based driver handles). Kept as the marker `Ident` for
    /// span-attached errors (`local` composes with neither `executor:` nor a
    /// multi-core provider — see `parse_node`).
    local: Option<Ident>,
    /// `consume` marker: the worker receives the value **by value** and the shell
    /// emits no restore — the slot is left empty when the worker exits, so a
    /// respawn gates on an explicit re-`provide()`. For resources that must be
    /// *dropped* at teardown (a driver whose `Drop` releases pins/DMA) or that go
    /// stale across a power cycle and must be rebuilt each run.
    consume: Option<Ident>,
    /// `shared` marker: a fan-out slot for a `Copy` handle. The glue copies the
    /// value out non-destructively (`get()`), the worker receives it **by
    /// value**, no restore — so any number of nodes (and whole pools) may
    /// declare the SAME slot name (the static is emitted once; re-declarations
    /// must repeat kinds + type exactly). Mutually exclusive with `consume`.
    shared: Option<Ident>,
}

impl ResourceDecl {
    /// The kinds+type signature every re-declaration of a `shared` slot must
    /// repeat verbatim (compared as token strings — same-name shared slots are
    /// ONE static, so their declared shapes must agree).
    fn shared_signature(&self) -> String {
        let ty = &self.ty;
        format!(
            "{}shared {}",
            if self.local.is_some() { "local " } else { "" },
            quote!(#ty)
        )
    }
}

/// Peek whether the next token of a `resources:` entry is a kind *marker*
/// (`local` / `consume` / `shared`) rather than the start of the resource
/// `Type` itself. Contextual-keyword rule (no reserved words): the ident is a
/// marker only when something else of the entry still follows it — i.e. it is
/// NOT a marker when followed by `::` or `<` (it starts a path/generic type
/// like `local::Foo` or `local<T>`) or by `,` / end-of-list (it IS the whole
/// type, a type literally named `local`). Same fork-and-peek disambiguation as
/// the pool `policy:` type annotation.
fn peek_kind_marker(content: ParseStream) -> Option<Ident> {
    if !content.peek(syn::Ident) {
        return None;
    }
    let fork = content.fork();
    let ident: Ident = fork.parse().ok()?;
    if ident != "local" && ident != "consume" && ident != "shared" {
        return None;
    }
    if fork.is_empty() || fork.peek(Token![,]) || fork.peek(Token![::]) || fork.peek(Token![<]) {
        return None;
    }
    Some(ident)
}

/// `resources: [LED: Output<'static>, RUNNER: local consume Runner, …]`
fn parse_resource_list(input: ParseStream) -> SynResult<Vec<ResourceDecl>> {
    let content;
    bracketed!(content in input);
    let mut resources = Vec::new();
    while !content.is_empty() {
        let cfg = content.call(Attribute::parse_outer)?;
        let ident: Ident = content.parse()?;
        content.parse::<Token![:]>()?;
        // Kind markers between the colon and the type, order-free: `local`
        // plus at most one of `consume` / `shared`. A repeated marker is a
        // declaration bug; `consume` (exclusive take, slot empty after exit)
        // and `shared` (non-destructive fan-out copy) contradict each other.
        let mut local: Option<Ident> = None;
        let mut consume: Option<Ident> = None;
        let mut shared: Option<Ident> = None;
        while let Some(marker) = peek_kind_marker(&content) {
            content.parse::<Ident>()?; // commit the peeked marker
            // `local` is the one kind whose slot type carries an `unsafe impl
            // Sync` — injecting unsafe code is an explicit opt-in, so the
            // marker is rejected unless the (non-default) `local-resources`
            // feature forwarded by the supervisor crate is enabled.
            if marker == "local" && !cfg!(feature = "local-resources") {
                return Err(syn::Error::new_spanned(
                    &marker,
                    "`local` resources emit an `unsafe impl Sync` — opt in by \
                     enabling embassy-supervisor's `local-resources` feature",
                ));
            }
            let slot = if marker == "local" {
                &mut local
            } else if marker == "consume" {
                &mut consume
            } else {
                &mut shared
            };
            if slot.is_some() {
                return Err(syn::Error::new_spanned(
                    &marker,
                    format!("duplicate `{marker}` marker"),
                ));
            }
            *slot = Some(marker);
        }
        if let (Some(_), Some(s)) = (&consume, &shared) {
            return Err(syn::Error::new_spanned(
                s,
                "`consume` and `shared` are mutually exclusive — `consume` takes \
                 the single value out for one owner, `shared` copies it out to \
                 any number of consumers",
            ));
        }
        let ty: Type = content.parse()?;
        resources.push(ResourceDecl {
            cfg,
            ident,
            ty,
            local,
            consume,
            shared,
        });
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        }
    }
    Ok(resources)
}

struct NodeItem {
    cfg: Vec<Attribute>,
    ident: Ident,
    mode: Ident,
    deps: Vec<Dep>,
    /// `None` = a parked node the app spawns itself (neither `spawn:` nor `task:`).
    source: Option<TaskSource>,
    /// `pool_size: N` on a `task:` node — sizes the generated shell's `TaskPool`
    /// (headroom for a respawn while the previous instance is still draining).
    pool_size: Option<LitInt>,
    /// `resources: [NAME: Type, ..]` on a `task:` node — owned values threaded
    /// from `main` through macro-emitted `ResourceSlot` statics into the
    /// generated shell (which hands the worker `&mut Type` and restores the
    /// value on exit). Empty for `spawn:`/parked nodes (enforced at parse).
    resources: Vec<ResourceDecl>,
    disabled: bool,
    /// `executor: NAME` — spawn through the named [`SpawnerSlot`] (a
    /// `SendSpawner` the app registers at runtime) instead of the supervisor's
    /// own `Spawner`. `None` = the default executor.
    executor: Option<Ident>,
    /// `slot_timeout: N` (milliseconds) — overrides the node's pre-spawn
    /// slot/gate wait bound (default 100 ms). Needed when the node's resources
    /// are filled by a **provider node** at runtime: size it to the provider's
    /// async build time.
    slot_timeout: Option<LitInt>,
    /// `exit: Type` on a `task:` node — the worker's return value is
    /// `provide()`d into a generated `pub static <NODE>_EXIT: ResourceSlot<Type>`
    /// just before the shell records the exit, so `<NODE>_EXIT.wait_take()`
    /// observes the completion value (idiomatically `Result<R, Aborted>` out of
    /// `run_cancellable` for completed-vs-cancelled). A worker that can never
    /// return rejects the clause: the provide is dead code, and the shell denies
    /// `unreachable_code` on it (spanned here) rather than emit a slot nothing
    /// could ever fill.
    exit: Option<syn::Type>,
    /// `state: Type = init_expr` (feature `heap-state`) — per-activation heap
    /// state: the glue fallibly boxes `init_expr` (alloc failure =
    /// `SpawnError::Busy`, retryable), the shell lends the worker `&mut Type`,
    /// and the Box drops on task exit — allocated fresh each activation,
    /// reclaimed on every exit.
    state: Option<(syn::Type, Expr)>,
    /// `cancel` on a `task:` node — the shell drives the worker under
    /// [`TaskNode::run_cancellable`] instead of awaiting it directly, and does
    /// NOT pass the node to it. For the common shape the supervisor otherwise
    /// can't take: a plain `async fn` that loops forever and knows nothing about
    /// any supervisor. On shutdown its future is dropped in place and the shell
    /// runs its usual restore/exit-provide/`mark_exited` tail.
    cancel: bool,
    /// The `supervisor_fragment!` this item was forwarded from (via the
    /// `@fragment NAME;` marker), for error attribution across the relay.
    fragment: Option<String>,
}

/// `executor NAME;` — declares a `pub static NAME: SpawnerSlot` the application
/// fills with a `SendSpawner` before (or concurrently with) `Supervisor::start` (an
/// InterruptExecutor tier, core1, ...). Nodes reference it with `executor: NAME`; the
/// supervisor awaits the slot before spawning such a node (bounded by
/// `SLOT_READY_TIMEOUT`, then `SpawnError::Busy`).
struct ExecutorItem {
    cfg: Vec<Attribute>,
    ident: Ident,
}

struct PoolItem {
    cfg: Vec<Attribute>,
    ident: Ident,
    modes: Vec<Ident>,
    deps: Vec<Dep>,
    /// The member task. Either a bare path (`http_task`) or a partial call carrying
    /// extra args (`mcp_server_task(stack())`); the macro spawns member `j` as
    /// `s.spawn(<fn>(&POOL[j] [, extra args])?)` — the node is always the first arg.
    /// No closure form (members are instantiated per index), unlike a node's `spawn:`.
    /// The `Shell` variant (`task:`) wraps a plain — possibly generic — async fn in
    /// ONE generated `#[embassy_executor::task(pool_size = K)]` shell shared by all
    /// members (they share one concrete future type, like a `spawn:` pool).
    source: TaskSource,
    /// The scaling policy value, emitted as the `policy:` field of the `ElasticPool`
    /// static. The static is typed `ElasticPool<P>`, so the macro needs the policy
    /// *type* `P`: when `policy_ty` is `None` it derives `P` from this expr via
    /// `policy_type` (requires a `Type::new(..)` shape); when `policy_ty` is `Some`
    /// the caller stated `P` explicitly and this expr can be any value of that type.
    policy: Expr,
    /// Optional explicit policy type from the `policy: <Type> = <expr>` form. `Some`
    /// bypasses `policy_type` derivation, allowing a value the deriver can't handle
    /// (a free fn, a const, a builder chain, a qualified path).
    policy_ty: Option<Type>,
    /// `executor: NAME` — spawn every member through the named [`SpawnerSlot`]
    /// (e.g. a worker pool on the second core, scaled by this core's supervisor).
    executor: Option<Ident>,
    /// Pool `resources:` — **`shared` entries only** (enforced at parse): each
    /// member's glue copies the same `Copy` handle out non-destructively, so
    /// members don't contend (the reason non-shared kinds stay rejected).
    resources: Vec<ResourceDecl>,
    /// `slot_timeout: N` (milliseconds) — applied to every member (see the
    /// node field of the same name).
    slot_timeout: Option<LitInt>,
    /// `min:`/`max:` — the scaling floor/ceiling. Any const-evaluable `usize`
    /// expression: integer literals validate at parse time (best spans); other
    /// exprs become the emitted `<POOL>_MIN`/`<POOL>_MAX` consts guarded by
    /// const asserts (min <= max <= member count <= 255). The member count
    /// itself (the mode list) stays structural — a proc macro cannot
    /// const-evaluate, and the count drives how many nodes/shells/names are
    /// emitted, so it cannot come from a const.
    min: Expr,
    max: Expr,
    /// `state: Type = init_expr` (feature `heap-state`) — per-activation heap
    /// state, one fresh Box per member per activation (see the node field).
    state: Option<(syn::Type, Expr)>,
    /// `cancel` on a `task:` pool — the one shared shell drives each member's
    /// worker under [`TaskNode::run_cancellable`] and does not lead its arguments
    /// with `&POOL[I]`, so a plain worker is shrunk (and torn down) by having its
    /// future dropped in place. See the node field of the same name.
    cancel: bool,
    /// The `supervisor_fragment!` this item was forwarded from (via the
    /// `@fragment NAME;` marker), for error attribution across the relay.
    fragment: Option<String>,
}

// Both variants embed a large `syn::Expr` (and `PoolItem` a bit more), so their sizes
// are close but unequal — enough for `large_enum_variant` to flag the gap. This AST is
// parsed once and lives only briefly in a `Vec` during expansion, so boxing a variant
// to shave a few bytes per element buys nothing real; suppress the lint instead of
// paying a heap allocation.
#[allow(clippy::large_enum_variant)]
enum Item {
    Node(NodeItem),
    Pool(PoolItem),
    Executor(ExecutorItem),
}

/// The parsed macro input: the list of `node`/`pool` declarations, in source order.
/// Named `GraphSpec` (not `Graph`) to stay distinct from the *emitted* public type
/// [`embassy_supervisor::Graph`] that `expand` produces as the `GRAPH` static.
struct GraphSpec {
    /// `name: IDENT;` as the FIRST item — the emitted graph static's ident
    /// (default `GRAPH`). Named graphs suffix every generated helper ident so
    /// several graphs coexist (even in one module); only the UNNAMED graph may
    /// emit the once-per-binary `_embassy_trace_*` hook symbols.
    name: Option<Ident>,
    items: Vec<Item>,
}

/// An item's `resources:` entries (nodes and pools both carry them; an
/// `executor` slot has none) — for the graph-wide pre-passes in `expand`.
fn item_resources(item: &Item) -> &[ResourceDecl] {
    match item {
        Item::Node(n) => &n.resources,
        Item::Pool(p) => &p.resources,
        Item::Executor(_) => &[],
    }
}

/// An item's own name + cfg attributes (for shared-slot bookkeeping/docs).
fn item_ident_cfg(item: &Item) -> Option<(&Ident, &[Attribute])> {
    match item {
        Item::Node(n) => Some((&n.ident, &n.cfg)),
        Item::Pool(p) => Some((&p.ident, &p.cfg)),
        Item::Executor(_) => None,
    }
}

impl Parse for GraphSpec {
    fn parse(input: ParseStream) -> SynResult<Self> {
        // Optional `name: IDENT;` first (same shape as a fragment's header).
        let name = if input.peek(kw::name) && input.peek2(Token![:]) {
            input.parse::<kw::name>()?;
            input.parse::<Token![:]>()?;
            let n: Ident = input.parse()?;
            input.parse::<Token![;]>()?;
            Some(n)
        } else {
            None
        };
        let mut items = Vec::new();
        // Set while parsing items forwarded through a `supervisor_fragment!`
        // relay: `@fragment NAME;` opens the span, `@endfragment;` closes it.
        // Purely for error attribution — the items themselves are ordinary.
        let mut current_fragment: Option<String> = None;
        while !input.is_empty() {
            if input.peek(Token![@]) {
                input.parse::<Token![@]>()?;
                if input.peek(kw::fragment) {
                    input.parse::<kw::fragment>()?;
                    current_fragment = Some(input.parse::<Ident>()?.to_string());
                } else if input.peek(kw::endfragment) {
                    input.parse::<kw::endfragment>()?;
                    current_fragment = None;
                } else {
                    return Err(input.error("expected `@fragment NAME;` or `@endfragment;`"));
                }
                input.parse::<Token![;]>()?;
                continue;
            }
            let cfg = input.call(Attribute::parse_outer)?;
            if input.peek(kw::node) {
                let mut n = parse_node(input, cfg)?;
                n.fragment = current_fragment.clone();
                items.push(Item::Node(n));
            } else if input.peek(kw::pool) {
                let mut p = parse_pool(input, cfg)?;
                p.fragment = current_fragment.clone();
                items.push(Item::Pool(p));
            } else if input.peek(kw::executor) {
                // `executor NAME;` — a runtime-filled SendSpawner slot; nodes
                // carrying `executor: NAME` spawn through it (the supervisor awaits
                // the slot before spawning them).
                input.parse::<kw::executor>()?;
                let ident: Ident = input.parse()?;
                input.parse::<Token![;]>()?;
                items.push(Item::Executor(ExecutorItem { cfg, ident }));
            } else {
                return Err(input.error(
                    "expected `node`, `pool`, or `executor` (optionally `#[cfg(...)]`-prefixed)",
                ));
            }
        }
        Ok(GraphSpec { name, items })
    }
}

// node IDENT = MODE, deps: [..] [, spawn: <expr>] [, disabled];
fn parse_node(input: ParseStream, cfg: Vec<Attribute>) -> SynResult<NodeItem> {
    input.parse::<kw::node>()?;
    let ident: Ident = input.parse()?;
    input.parse::<Token![=]>()?;
    let mode: Ident = input.parse()?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::deps>()?;
    input.parse::<Token![:]>()?;
    let deps = parse_dep_list(input)?;

    let mut spawn = None;
    let mut task: Option<(kw::task, Expr)> = None;
    let mut pool_size = None;
    let mut disabled = false;
    let mut executor = None;
    let mut resources: Option<(kw::resources, Vec<ResourceDecl>)> = None;
    let mut slot_timeout = None;
    let mut exit: Option<(kw::exit, syn::Type)> = None;
    let mut state: Option<(kw::state, syn::Type, Expr)> = None;
    let mut cancel: Option<kw::cancel> = None;
    while input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
        if input.peek(kw::spawn) {
            input.parse::<kw::spawn>()?;
            input.parse::<Token![:]>()?;
            spawn = Some(input.parse::<Expr>()?);
        } else if input.peek(kw::task) {
            let k = input.parse::<kw::task>()?;
            input.parse::<Token![:]>()?;
            task = Some((k, input.parse::<Expr>()?));
        } else if input.peek(kw::pool_size) {
            input.parse::<kw::pool_size>()?;
            input.parse::<Token![:]>()?;
            pool_size = Some(input.parse::<LitInt>()?);
        } else if input.peek(kw::disabled) {
            input.parse::<kw::disabled>()?;
            disabled = true;
        } else if input.peek(kw::cancel) {
            cancel = Some(input.parse::<kw::cancel>()?);
        } else if input.peek(kw::executor) {
            input.parse::<kw::executor>()?;
            input.parse::<Token![:]>()?;
            executor = Some(input.parse::<Ident>()?);
        } else if input.peek(kw::resources) {
            let k = input.parse::<kw::resources>()?;
            input.parse::<Token![:]>()?;
            resources = Some((k, parse_resource_list(input)?));
        } else if input.peek(kw::slot_timeout) {
            input.parse::<kw::slot_timeout>()?;
            input.parse::<Token![:]>()?;
            slot_timeout = Some(input.parse::<LitInt>()?);
        } else if input.peek(kw::exit) {
            let k = input.parse::<kw::exit>()?;
            input.parse::<Token![:]>()?;
            exit = Some((k, input.parse::<syn::Type>()?));
        } else if input.peek(kw::state) {
            let k = input.parse::<kw::state>()?;
            input.parse::<Token![:]>()?;
            let ty: syn::Type = input.parse()?;
            input.parse::<Token![=]>()?;
            let init: Expr = input.parse()?;
            if !cfg!(feature = "heap-state") {
                return Err(syn::Error::new_spanned(
                    k,
                    "`state:` requires the `heap-state` feature \
                     (embassy-supervisor feature `heap-state`) — per-activation \
                     boxed state, reclaimed on task exit",
                ));
            }
            state = Some((k, ty, init));
        } else {
            return Err(input.error(
                "expected `spawn:`, `task:`, `pool_size:`, `executor:`, `resources:`, \
                 `slot_timeout:`, `exit:`, `state:`, `cancel`, or `disabled`",
            ));
        }
    }
    input.parse::<Token![;]>()?;

    // `slot_timeout: 0` would make every gated spawn fail instantly — reject it
    // as the declaration bug it is (`base10_parse::<u64>` also rejects suffixed
    // or oversized literals with a span-attached error).
    if let Some(st) = &slot_timeout {
        if st.base10_parse::<u64>()? == 0 {
            return Err(syn::Error::new_spanned(
                st,
                "`slot_timeout:` must be at least 1 (milliseconds)",
            ));
        }
    }

    // Exactly one of `spawn:` / `task:` may pick the node's task.
    if let (Some(_), Some((k, _))) = (&spawn, &task) {
        return Err(syn::Error::new_spanned(
            k,
            "`task:` and `spawn:` are mutually exclusive — `spawn:` names a \
             hand-written `#[embassy_executor::task]` fn, `task:` generates one",
        ));
    }
    // `pool_size:` sizes the generated shell's TaskPool; without `task:` there is
    // no generated shell to size (a `spawn:` task fn declares its own).
    if let (Some(ps), None) = (&pool_size, &task) {
        return Err(syn::Error::new_spanned(
            ps,
            "`pool_size:` requires `task:` — a `spawn:` task fn sets its own \
             `#[embassy_executor::task(pool_size = ...)]`",
        ));
    }
    // `resources:` only makes sense with `task:`: the generated shell is what
    // takes the values out of their slots at spawn and restores them after the
    // worker returns. A hand-written `spawn:` fn (or a parked node) manages its
    // own arguments.
    if let Some((k, decls)) = &resources {
        if task.is_none() {
            return Err(syn::Error::new_spanned(
                k,
                "`resources:` requires `task:` — resources are handed to the \
                 generated shell as owned arguments and restored by it; a \
                 `spawn:` task fn manages its own arguments",
            ));
        }
        if decls.is_empty() {
            return Err(syn::Error::new_spanned(
                k,
                "`resources:` must declare at least one `NAME: Type` entry",
            ));
        }
        // Duplicate names within one node would emit two statics with the same
        // ident; catch it here with a clearer message than rustc's E0428.
        for (i, d) in decls.iter().enumerate() {
            if decls[..i].iter().any(|prev| prev.ident == d.ident) {
                return Err(syn::Error::new_spanned(
                    &d.ident,
                    format!("duplicate resource name `{}`", d.ident),
                ));
            }
        }
    }
    if let Some(ps) = &pool_size {
        if ps.base10_parse::<usize>()? == 0 {
            return Err(syn::Error::new_spanned(
                ps,
                "`pool_size:` must be at least 1",
            ));
        }
    }
    // `state:` lives in the generated shell (it owns the Box across the worker
    // call and drops it on exit); a `spawn:` fn can box its own state.
    if let Some((k, _, _)) = &state
        && task.is_none()
    {
        return Err(syn::Error::new_spanned(
            k,
            "`state:` requires `task:` — the generated shell owns the boxed \
             state across the worker call and drops it on exit; a `spawn:` \
             task fn can Box its own state",
        ));
    }
    // `exit:` captures the worker's return value in the generated shell — only
    // `task:` has one. A `spawn:` fn (or a parked node) owns its body and can
    // `provide()` into any slot itself.
    if let Some((k, _)) = &exit {
        if task.is_none() {
            return Err(syn::Error::new_spanned(
                k,
                "`exit:` requires `task:` — the generated shell is what captures \
                 the worker's return value; a `spawn:` task fn can provide() into \
                 a slot itself",
            ));
        }
    }
    // `cancel` rewrites how the generated shell drives the worker; a `spawn:` fn
    // (or a parked node) owns its own body and can call `run_cancellable` itself.
    if let Some(k) = &cancel {
        if task.is_none() {
            return Err(syn::Error::new_spanned(
                k,
                "`cancel` requires `task:` — it wraps the generated shell's call \
                 to the worker; a `spawn:` task fn can call \
                 `node.run_cancellable(..)` itself",
            ));
        }
        // A Pause worker is supposed to SURVIVE a stop (ack, park on
        // `wait_resume()`, keep its resources); `cancel` drops its future and
        // lets the shell record an exit, after which nothing resumes it. That is
        // the opposite of the mode, so reject the pair rather than silently
        // turning a Pause node into a one-shot.
        if mode == "Pause" {
            return Err(syn::Error::new_spanned(
                k,
                "`cancel` cannot be combined with `Mode::Pause` — a Pause worker \
                 must survive the stop and park on `wait_resume()`, but `cancel` \
                 drops its future and records an exit; use `Mode::Terminate` (or \
                 `OnDemand`), or drive the pause by hand in the worker",
            ));
        }
    }
    // A `local` resource makes the shell future hold a `!Send`-capable value, and
    // an `executor:`-routed node spawns through a `SendSpawner`, whose `spawn`
    // requires a `Send` future. Reject here with the reason instead of letting
    // rustc surface it as an opaque `F: Send` bound failure deep in the glue.
    if let (Some((_, decls)), Some(ex)) = (&resources, &executor) {
        if let Some(l) = decls.iter().find_map(|d| d.local.as_ref()) {
            return Err(syn::Error::new_spanned(
                l,
                format!(
                    "`local` resources cannot be combined with `executor: {ex}` — a \
                     local slot exists to carry `!Send` values, and a node routed \
                     through a `SpawnerSlot` (`SendSpawner`) must have a `Send` \
                     future; run the node on the supervisor's own executor"
                ),
            ));
        }
    }
    let source = match (spawn, task) {
        (Some(e), _) => Some(TaskSource::Spawn(e)),
        (None, Some((_, e))) => Some(TaskSource::Shell(e)),
        (None, None) => None,
    };

    Ok(NodeItem {
        cfg,
        ident,
        mode,
        deps,
        source,
        pool_size,
        disabled,
        executor,
        resources: resources.map(|(_, decls)| decls).unwrap_or_default(),
        slot_timeout,
        exit: exit.map(|(_, ty)| ty),
        state: state.map(|(_, ty, init)| (ty, init)),
        cancel: cancel.is_some(),
        fragment: None,
    })
}

// pool IDENT = [MODES], deps: [..][, executor: EXEC], spawn: <fn>, policy: EXPR, min: N, max: M;
fn parse_pool(input: ParseStream, cfg: Vec<Attribute>) -> SynResult<PoolItem> {
    input.parse::<kw::pool>()?;
    let ident: Ident = input.parse()?;
    input.parse::<Token![=]>()?;
    let modes = parse_mode_list(input)?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::deps>()?;
    input.parse::<Token![:]>()?;
    let deps = parse_dep_list(input)?;
    input.parse::<Token![,]>()?;
    // Optional `executor: NAME,` — run the whole pool on the named SpawnerSlot's
    // executor (e.g. a worker pool on the second core, scaled from this one).
    let executor = if input.peek(kw::executor) {
        input.parse::<kw::executor>()?;
        input.parse::<Token![:]>()?;
        let ex: Ident = input.parse()?;
        input.parse::<Token![,]>()?;
        Some(ex)
    } else {
        None
    };
    // The member task: a path, or a partial call supplying extra args (the macro
    // injects `&POOL[j]` as the first argument in either case). `spawn:` names a
    // hand-written `#[embassy_executor::task(pool_size = K)]` fn; `task:` names a
    // plain (possibly generic) async fn the macro wraps in ONE generated shell
    // task sized `pool_size = K`.
    let source = if input.peek(kw::task) {
        input.parse::<kw::task>()?;
        input.parse::<Token![:]>()?;
        TaskSource::Shell(input.parse()?)
    } else {
        input.parse::<kw::spawn>()?;
        input.parse::<Token![:]>()?;
        TaskSource::Spawn(input.parse()?)
    };
    input.parse::<Token![,]>()?;
    // Pool `resources:` — `shared` entries only. A take-kind slot holds ONE
    // value and pool members all run the same worker: they would contend for
    // that single instance and every member past the first would fail its
    // spawn. A `shared` entry is a non-destructive fan-out copy, so members
    // don't contend — each glue `get()`s the same `Copy` handle.
    let resources = if input.peek(kw::resources) {
        input.parse::<kw::resources>()?;
        input.parse::<Token![:]>()?;
        let decls = parse_resource_list(input)?;
        // Take-kind entries (lend/consume) become per-member SLOT ARRAYS
        // (`[ResourceSlot<T>; K]`, member `I` takes/restores index `I`), so
        // members no longer contend. Only TAKE-KIND `local` stays rejected:
        // its single-core provide/take/restore contract interacts with
        // per-member restore in ways deferred for now. A `shared local`
        // entry is fine — it rides the pool-wide shared-slot path (one
        // graph-site slot, non-destructive `get()`, no restore), exactly as
        // before per-member resources existed.
        if let Some(bad) = decls
            .iter()
            .find(|d| d.local.is_some() && d.shared.is_none())
        {
            return Err(syn::Error::new_spanned(
                &bad.ident,
                "`local` is not supported on take-kind `pool` resources (the single-core \
                 slot contract + per-member restore is deferred); a `shared local` entry \
                 works (one pool-wide fan-out slot), or declare the take-kind `local` \
                 resource on a node",
            ));
        }
        input.parse::<Token![,]>()?;
        decls
    } else {
        Vec::new()
    };
    // Optional `state: Type = expr,` — per-member per-activation boxed state.
    let state = if input.peek(kw::state) {
        let k = input.parse::<kw::state>()?;
        input.parse::<Token![:]>()?;
        let ty: syn::Type = input.parse()?;
        input.parse::<Token![=]>()?;
        let init: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        if !cfg!(feature = "heap-state") {
            return Err(syn::Error::new_spanned(
                k,
                "`state:` requires the `heap-state` feature \
                 (embassy-supervisor feature `heap-state`) — per-activation \
                 boxed state, reclaimed on task exit",
            ));
        }
        Some((ty, init))
    } else {
        None
    };
    // `exit:` would land here positionally; reject it with the reason instead of
    // the generic "expected `policy`" the positional grammar produces.
    if input.peek(kw::exit) {
        let k = input.parse::<kw::exit>()?;
        return Err(syn::Error::new_spanned(
            k,
            "`exit:` is not supported on `pool` — the K members share one shell, \
             so per-member exit values need per-member storage; use per-node \
             `exit:` declarations, or have the worker provide() into an \
             app-declared slot itself",
        ));
    }
    input.parse::<kw::policy>()?;
    input.parse::<Token![:]>()?;
    // Optional explicit policy type: `policy: <Ty> = <expr>`. Fork to see if a `Type`
    // is followed by `=`; if so it's an annotation (commit on the real stream + eat the
    // `=`), otherwise rewind and treat the whole thing as the value expr (type derived
    // from it in `emit_pool`). For the common `Ty::new(..)` value the fork parses only a
    // partial type and then sees `(`, not `=`, so it correctly falls back to the derive
    // path — this keeps the bare form working unchanged.
    let policy_ty = {
        let fork = input.fork();
        if fork.parse::<Type>().is_ok() && fork.peek(Token![=]) {
            let ty: Type = input.parse()?;
            input.parse::<Token![=]>()?;
            Some(ty)
        } else {
            None
        }
    };
    let policy: Expr = input.parse()?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::min>()?;
    input.parse::<Token![:]>()?;
    let min: Expr = input.parse()?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::max>()?;
    input.parse::<Token![:]>()?;
    let max: Expr = input.parse()?;
    // Optional trailing `, slot_timeout: N` (milliseconds, ≥ 1) — every member's
    // pre-spawn slot/gate wait bound (see the node clause of the same name).
    let slot_timeout = if input.peek(Token![,]) && input.peek2(kw::slot_timeout) {
        input.parse::<Token![,]>()?;
        input.parse::<kw::slot_timeout>()?;
        input.parse::<Token![:]>()?;
        let st: LitInt = input.parse()?;
        if st.base10_parse::<u64>()? == 0 {
            return Err(syn::Error::new_spanned(
                &st,
                "`slot_timeout:` must be at least 1 (milliseconds)",
            ));
        }
        Some(st)
    } else {
        None
    };
    // Optional trailing `, cancel` — the pool's one shared shell owns the
    // shutdown race for every member (see the node clause of the same name).
    // Shrink and teardown already signal each member; `cancel` is what makes a
    // worker that never returns answer them.
    let cancel = if input.peek(Token![,]) && input.peek2(kw::cancel) {
        input.parse::<Token![,]>()?;
        Some(input.parse::<kw::cancel>()?)
    } else {
        None
    };
    input.parse::<Token![;]>()?;
    if let Some(k) = &cancel {
        // Same reason as the node: `cancel` rewrites how the GENERATED shell
        // drives the worker, so there must be one.
        if matches!(source, TaskSource::Spawn(_)) {
            return Err(syn::Error::new_spanned(
                k,
                "`cancel` requires `task:` — it wraps the generated shell's call to \
                 the member worker; a `spawn:` member fn can call \
                 `node.run_cancellable(..)` itself",
            ));
        }
        // A Pause member must survive its stop and park on `wait_resume()`;
        // `cancel` drops its future and records an exit instead. Reject the pair
        // per member rather than silently turning parked members into one-shots.
        if let Some(m) = modes.iter().find(|m| *m == "Pause") {
            return Err(syn::Error::new_spanned(
                m,
                "`cancel` cannot be combined with a `Pause` member — a Pause worker \
                 must survive the stop and park on `wait_resume()`, but `cancel` \
                 drops its future and records an exit; use `Terminate` (or \
                 `OnDemand`) members, or drive the pause by hand in the worker",
            ));
        }
    }
    // Same `Send` reasoning as the node-side check: a `local` (i.e. `!Send`able)
    // resource cannot ride members routed through a `SendSpawner`.
    if let Some(ex) = &executor {
        if let Some(l) = resources.iter().find_map(|d| d.local.as_ref()) {
            return Err(syn::Error::new_spanned(
                l,
                format!(
                    "`local` resources cannot be combined with `executor: {ex}` — a \
                     local slot exists to carry `!Send` values, and a pool routed \
                     through a `SpawnerSlot` (`SendSpawner`) must have `Send` \
                     futures; run the pool on the supervisor's own executor"
                ),
            ));
        }
    }
    Ok(PoolItem {
        cfg,
        ident,
        modes,
        deps,
        source,
        policy,
        policy_ty,
        executor,
        resources,
        slot_timeout,
        min,
        max,
        state,
        cancel: cancel.is_some(),
        fragment: None,
    })
}

/// The node/pool name string: ident lowercased with `_`→`-` (`WIFI_CTRL` → "wifi-ctrl").
fn name_string(ident: &Ident) -> String {
    ident.to_string().to_lowercase().replace('_', "-")
}

/// Build a task-call expression with leading arguments injected ahead of the
/// user-supplied extras — the node ref (`&NODE` / `&POOL[i]`) first, then the
/// item's threaded `resources:` values: a bare path `f` => `f(lead..)`; a
/// partial call `f(a, b)` => `f(lead.., a, b)`.
///
/// The lead may be EMPTY: a `cancel` shell suppresses the node ref, so a node
/// with no `resources:`/`state:` leads with nothing at all. Both groups are
/// therefore joined as ONE list — a separator hard-coded between them would
/// emit `f(, a, b)`.
fn inject_call_with(task: &Expr, lead: &[TokenStream2]) -> SynResult<TokenStream2> {
    match task {
        Expr::Path(_) => Ok(quote!(#task(#(#lead),*))),
        Expr::Call(c) => {
            let f = &c.func;
            let mut args: Vec<TokenStream2> = lead.to_vec();
            args.extend(c.args.iter().map(|a| quote!(#a)));
            Ok(quote!(#f(#(#args),*)))
        }
        other => Err(syn::Error::new_spanned(
            other,
            "expected a task-fn path or a partial call like `f(extra_args)`",
        )),
    }
}

/// Combine an item's `#[cfg(...)]` attributes into one predicate (`all(..)` if
/// several), used to gate its `GRAPH.nodes` slot to `Some`/`None`. `None` = always present.
fn cfg_predicate(attrs: &[Attribute]) -> Option<TokenStream2> {
    let preds: Vec<TokenStream2> = attrs
        .iter()
        .filter_map(|a| match &a.meta {
            Meta::List(ml) if ml.path.is_ident("cfg") => Some(ml.tokens.clone()),
            _ => None,
        })
        .collect();
    match preds.len() {
        0 => None,
        1 => Some(preds[0].clone()),
        _ => Some(quote!(all(#(#preds),*))),
    }
}

/// Gate-array tokens for a `resources:` list: the element list (each entry
/// `#[cfg]`-gated — cfg on array elements is stable, same as the deps table)
/// and a matching length expression. A cfg'd-out element must also subtract
/// from the fixed array length, so with any per-entry cfg the length becomes a
/// sum of cfg-block 1/0 terms (the `GRAPH.nodes` Some/None trick, in const
/// position); without, it stays the plain count.
fn gate_tokens(resources: &[ResourceDecl]) -> (TokenStream2, Vec<TokenStream2>) {
    let gate_refs: Vec<TokenStream2> = resources
        .iter()
        .map(|r| {
            let cfg = &r.cfg;
            let res = &r.ident;
            quote!(#(#cfg)* &#res)
        })
        .collect();
    let any_cfg = resources.iter().any(|r| cfg_predicate(&r.cfg).is_some());
    let len = if any_cfg {
        let terms: Vec<TokenStream2> = resources
            .iter()
            .map(|r| match cfg_predicate(&r.cfg) {
                None => quote!(1usize),
                Some(pred) => quote!({
                    #[cfg(#pred)]
                    {
                        1usize
                    }
                    #[cfg(not(#pred))]
                    {
                        0usize
                    }
                }),
            })
            .collect();
        quote!(0usize #(+ #terms)*)
    } else {
        let n = resources.len();
        quote!(#n)
    };
    (len, gate_refs)
}

/// `" (from fragment \`X\`)"` when the item was forwarded through a
/// `supervisor_fragment!` relay, else empty — error-message attribution.
fn fragment_suffix(fragment: &Option<String>) -> String {
    match fragment {
        Some(f) => format!(" (from fragment `{f}`)"),
        None => String::new(),
    }
}

/// Build the `[&'static TaskNode; n]` element and length tokens for a node's or
/// pool's `ready`-marked deps, cfg-aware like `gate_tokens`. A dep naming a pool
/// resolves to the pool's floor member (`&POOL[0]`), matching how `deps: [POOL]`
/// resolves for spawn ordering.
fn ready_tokens(
    deps: &[Dep],
    pool_names: &std::collections::HashSet<String>,
) -> Option<(TokenStream2, Vec<TokenStream2>)> {
    let marked: Vec<&Dep> = deps.iter().filter(|d| d.ready.is_some()).collect();
    if marked.is_empty() {
        return None;
    }
    let refs: Vec<TokenStream2> = marked
        .iter()
        .map(|d| {
            let cfg = &d.cfg;
            let ident = &d.ident;
            if pool_names.contains(&ident.to_string()) {
                quote!(#(#cfg)* &#ident[0])
            } else {
                quote!(#(#cfg)* &#ident)
            }
        })
        .collect();
    let any_cfg = marked.iter().any(|d| cfg_predicate(&d.cfg).is_some());
    let len = if any_cfg {
        let terms: Vec<TokenStream2> = marked
            .iter()
            .map(|d| match cfg_predicate(&d.cfg) {
                None => quote!(1usize),
                Some(pred) => quote!({
                    #[cfg(#pred)]
                    {
                        1usize
                    }
                    #[cfg(not(#pred))]
                    {
                        0usize
                    }
                }),
            })
            .collect();
        quote!(0usize #(+ #terms)*)
    } else {
        let n = marked.len();
        quote!(#n)
    };
    Some((len, refs))
}

/// Extract the policy *type* from a `Type::new(..)` constructor expression. Only used
/// on the derive path (no explicit `policy: <Ty> = ..` annotation); the type is the
/// call's path minus its last segment (`DeferredShrink::new` -> `DeferredShrink`).
fn policy_type(expr: &Expr) -> SynResult<Path> {
    if let Expr::Call(call) = expr
        && let Expr::Path(p) = &*call.func
    {
        let n = p.path.segments.len();
        if n >= 2 {
            let segs: Punctuated<_, Token![::]> =
                p.path.segments.iter().take(n - 1).cloned().collect();
            return Ok(Path {
                leading_colon: p.path.leading_colon,
                segments: segs,
            });
        }
    }
    Err(syn::Error::new_spanned(
        expr,
        "pool `policy:` must be a `Type::new(..)` constructor (e.g. `DeferredShrink::new(..)`), \
         or give the type explicitly: `policy: <Type> = <expr>`",
    ))
}

/// One emitted node slot, in final index order.
struct Slot {
    /// Presence predicate (`None` = unconditional), gates the node slot (`GRAPH.nodes`) entry.
    cfg_pred: Option<TokenStream2>,
    /// `&NODE` or `&POOL[j]`.
    reference: TokenStream2,
    /// Raw deps, resolved to indices in the second pass.
    deps: Vec<Dep>,
    /// The `supervisor_fragment!` the owning item came from, for error
    /// attribution when a dep fails to resolve across the relay.
    fragment: Option<String>,
}

/// The `Option<fn(..)>` spawn expression for a node. `None` (no `spawn:`) is a
/// parked node the app spawns itself. A path or partial call is a task fn taking
/// `&NODE` first (plus any given args); the macro wraps it as
/// `|s| { s.spawn(<task>(&NODE, ..)?); Ok(()) }`. Anything else (a closure, or a
/// ready spawn fn) is emitted verbatim. Every form is cast to `spawn_fn` so it
/// coerces cleanly inside `Option::Some(..)`.
fn node_spawn(
    ident: &Ident,
    spawn: &Option<Expr>,
    executor: &Option<Ident>,
    resources: &[ResourceDecl],
    // `state:`: fallibly box the init value in the glue, BEFORE the resource
    // takes (a failed alloc strands nothing) — `SpawnError::Busy`, retryable.
    state: Option<&(syn::Type, Expr)>,
    spawn_fn: &TokenStream2,
    helpers: &HelperIdents,
) -> SynResult<TokenStream2> {
    // `resources:` take-prelude + the taken values as extra shell arguments.
    // Taking here — in the glue, BEFORE the spawn — is the point: an unprovided
    // slot fails `Supervisor::start` with `SpawnError::Busy` (the supervisor
    // logs the node name), instead of panicking inside an already-spawned task.
    // The values ride into the task as ordinary `#[embassy_executor::task]`
    // arguments (embassy stores them in the shell's TaskPool slot). A `shared`
    // entry copies the value out non-destructively (`get()` — the slot stays
    // filled for the other consumers) instead of `take()`ing it; `get`'s
    // `T: Copy` bound is what enforces "shared handles must be Copy".
    let take_prelude: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let cfg = &r.cfg;
            let res = &r.ident;
            let var = format_ident!("__r{}", i);
            let getter = if r.shared.is_some() {
                quote!(get)
            } else {
                quote!(take)
            };
            quote! {
                #(#cfg)*
                let #var = #res
                    .#getter()
                    .ok_or(::embassy_executor::SpawnError::Busy)?;
            }
        })
        .collect();
    // Per-entry `#[cfg]` rides on the call ARGUMENT too (stable in call
    // position, like the cfg'd array elements in the deps table), so a
    // cfg'd-out entry vanishes from the glue, the shell signature, and the
    // worker call consistently.
    let res_args: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let cfg = &r.cfg;
            let var = format_ident!("__r{}", i);
            quote!(#(#cfg)* #var)
        })
        .collect();
    let try_box = &helpers.try_box;
    let (state_prelude, state_arg) = match state {
        Some((_, init)) => (
            quote! {
                let __state = #try_box(#init)
                    .ok_or(::embassy_executor::SpawnError::Busy)?;
            },
            vec![quote!(__state)],
        ),
        None => (quote!(), vec![]),
    };
    Ok(match (spawn, executor) {
        (None, None) => quote!(::core::option::Option::None),
        // `executor:` needs the macro to perform the spawn, so it composes only
        // with the path / partial-call `spawn:` forms below.
        (None, Some(ex)) => {
            return Err(syn::Error::new_spanned(
                ex,
                "`executor:` requires a `spawn:` (a parked node is spawned by the \
                 application, which picks its own spawner)",
            ));
        }
        // A path or a partial call: a task fn taking `&NODE` first (plus any
        // given args); generate `|s| { s.spawn(<task>(&NODE, ..)?); Ok(()) }`.
        // With `executor: NAME` the glue ignores the supervisor's `Spawner` and
        // spawns through the named `SpawnerSlot` (a `SendSpawner` the app
        // registers at runtime): an unfilled slot fails the spawn with
        // `SpawnError::Busy` — loud misconfiguration, not a missing task. The
        // task future must then be `Send` (enforced by `SendSpawner::spawn`).
        (Some(e @ (Expr::Path(_) | Expr::Call(_))), executor) => {
            let mut lead: Vec<TokenStream2> = vec![quote!(&#ident)];
            lead.extend(res_args.iter().cloned());
            lead.extend(state_arg.iter().cloned());
            let call = inject_call_with(e, &lead)?;
            match executor {
                None => {
                    let stmts = spawn_stmts(&call, &quote!(&#ident), &quote!(s));
                    quote!(::core::option::Option::Some(
                        (|s| {
                            #state_prelude
                            #(#take_prelude)*
                            #stmts
                            ::core::result::Result::Ok(())
                        }) as #spawn_fn
                    ))
                }
                Some(ex) => {
                    let stmts = spawn_stmts(&call, &quote!(&#ident), &quote!(__sp));
                    quote!(::core::option::Option::Some(
                        (|_s| {
                            // The supervisor awaits this slot's `ready()` before
                            // invoking the glue (the node carries `.with_executor(&EX)`
                            // and the bring-up bounds the wait), so `get()` is already
                            // filled; `ok_or` is the belt-and-braces unfilled guard.
                            // Resources are taken AFTER the spawner guard, so an
                            // unfilled executor never consumes (and strands) them.
                            let __sp = #ex
                                .get()
                                .ok_or(::embassy_executor::SpawnError::Busy)?;
                            #state_prelude
                            #(#take_prelude)*
                            #stmts
                            ::core::result::Result::Ok(())
                        }) as #spawn_fn
                    ))
                }
            }
        }
        (Some(_), Some(ex)) => {
            return Err(syn::Error::new_spanned(
                ex,
                "`executor:` cannot be combined with a verbatim spawn closure (the \
                 closure owns the spawn; use the named SpawnerSlot inside it instead)",
            ));
        }
        // Anything else (a closure, or a ready spawn fn) is emitted verbatim.
        // NOTE: with the `trace` feature such a node is not auto-mapped — the
        // closure owns the SpawnToken; call `adopt`/`set_task_id` in it yourself.
        (Some(e), None) => quote!(::core::option::Option::Some((#e) as #spawn_fn)),
    })
}

/// The spawn statement(s) for the generated glue. Plain `s.spawn(<call>?)`
/// normally; with the `trace` feature the `SpawnToken` is bound first so its task
/// id can be captured into the node (`set_task_id`) — the id→node mapping the
/// supervisor's `trace` recorders resolve against (in embassy-executor 0.10 the
/// task-fn call returns `Result<SpawnToken, SpawnError>` and `Spawner::spawn`
/// itself is infallible, so the token is available between the two).
///
/// Three shapes, resolved at expansion by the macro crate's own features:
/// * `trace` on → bind the token and `adopt` it (`set_task_id` + name stamp under
///   `metadata-names`).
/// * `trace` off but `metadata-names` on → bind the token and `stamp_name` only:
///   the node name reaches the task Metadata (for rtos-trace/SystemView) with no id
///   capture and no dependency on the `_embassy_trace_*` hooks.
/// * neither → plain infallible spawn.
fn spawn_stmts(call: &TokenStream2, node_ref: &TokenStream2, sp: &TokenStream2) -> TokenStream2 {
    if cfg!(feature = "trace") {
        // `adopt` = set_task_id + (under metadata-names) Metadata name stamp.
        quote! {
            let __token = #call?;
            (#node_ref).adopt(&__token);
            #sp.spawn(__token);
        }
    } else if cfg!(feature = "metadata-names") {
        // Name-only path: stamp the node name into the task Metadata, nothing else.
        quote! {
            let __token = #call?;
            (#node_ref).stamp_name(&__token);
            #sp.spawn(__token);
        }
    } else {
        quote!(#sp.spawn(#call?);)
    }
}

/// Emit the `#[embassy_executor::task]` shell for a `task:` clause: a concrete,
/// non-generic task fn that takes only the node and awaits the user's worker with
/// the node injected first. This is how a **generic** worker becomes spawnable —
/// embassy forbids generic tasks (one static `TaskPool` per concrete future type),
/// so a monomorphized shell is stamped per declaration. Worker args are evaluated
/// inside the shell — at the task's first poll, on the node's own executor — so
/// the DSL never needs the arg types and a cross-core node builds its resources on
/// the core that runs them.
///
/// Returns the shell item and a path `Expr` naming it, which feeds the ordinary
/// `spawn:` path-form glue (executor routing and trace `adopt` compose unchanged).
// One argument per independent codegen input; a bundling struct would only
// rename the coupling.
#[allow(clippy::too_many_arguments)]
fn emit_shell(
    owner: &Ident,
    cfg: &[Attribute],
    worker: &Expr,
    pool_size: usize,
    resources: &[ResourceDecl],
    exit: Option<&syn::Type>,
    // `state: Type = ..`: the shell owns the glue-boxed state across the worker
    // call (worker sees `&mut Type`) and DROPS it first thing after the worker
    // returns — reclaimed before restores/exit-provide/mark_exited.
    state: Option<&(syn::Type, Expr)>,
    // Pool shells restore lend entries to a slot REFERENCE parameter (the
    // member's own array element, passed by the wrapper) instead of a slot
    // named statically — restore-to-same-index by construction.
    pool_member: bool,
    // `cancel`: drive the worker under `run_cancellable` and DON'T lead its
    // arguments with the node — the worker is a plain future that never returns
    // on its own, so the shell owns the shutdown race on its behalf.
    cancel: bool,
    cr: &TokenStream2,
    helpers: &HelperIdents,
) -> SynResult<(TokenStream2, Expr)> {
    if !matches!(worker, Expr::Path(_) | Expr::Call(_)) {
        return Err(syn::Error::new_spanned(
            worker,
            "`task:` names an async worker fn — a path or a partial call like \
             `worker(args)`; for a closure or a ready spawn fn use `spawn:`",
        ));
    }
    let shell = format_ident!("__sv_task_{}", owner.to_string().to_lowercase());
    // `resources:` values arrive as owned task arguments (the spawn glue took
    // them out of their slots); the shell keeps ownership, lends the worker
    // `&mut`, and restores each value to its slot after the worker returns —
    // i.e. after the worker's clean shutdown ack — so a Terminate respawn
    // re-takes the SAME instance instead of re-acquiring hardware. A `Pause`
    // worker parks instead of returning, so it simply retains its resources
    // (the restore lines below are unreachable for it — correct, same as a
    // hand-written parked task holding its arguments).
    //
    // A `consume` entry is forwarded to the worker BY VALUE instead — the worker
    // owns it (it can drop it at teardown, e.g. a driver whose `Drop` releases
    // pins/DMA) and no restore is emitted: the slot stays empty until the app
    // re-`provide()`s, which the supervisor's pre-respawn gate wait turns into
    // fail-closed `SpawnError::Busy` rather than a stale-value reuse.
    //
    // A `shared` entry is also by value with no restore — but because the glue
    // COPIED it out (`get()`), the slot stays filled; the worker's value is its
    // own copy of the fan-out handle.
    //
    // Per-entry `#[cfg]` rides on params, worker-call arguments, and restore
    // statements alike, so a cfg'd-out entry disappears from the whole chain
    // (the worker fn must gate its matching parameter with the same `#[cfg]`).
    let by_value = |r: &ResourceDecl| r.consume.is_some() || r.shared.is_some();
    let res_params: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let cfg = &r.cfg;
            let var = format_ident!("__r{}", i);
            let ty = &r.ty;
            if by_value(r) {
                quote!(#(#cfg)* #var: #ty)
            } else if pool_member {
                // Lend entry of a pool: value + the member's own slot element.
                let slot_param = format_ident!("__r{}_slot", i);
                quote!(#(#cfg)* mut #var: #ty, #(#cfg)* #slot_param: &'static #cr::ResourceSlot<#ty>)
            } else {
                quote!(#(#cfg)* mut #var: #ty)
            }
        })
        .collect();
    let res_leases: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let cfg = &r.cfg;
            let var = format_ident!("__r{}", i);
            if by_value(r) {
                quote!(#(#cfg)* #var)
            } else {
                quote!(#(#cfg)* &mut #var)
            }
        })
        .collect();
    let restores: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .filter(|(_, r)| !by_value(r))
        .map(|(i, r)| {
            let cfg = &r.cfg;
            let var = format_ident!("__r{}", i);
            if pool_member {
                let slot_param = format_ident!("__r{}_slot", i);
                quote!(#(#cfg)* #slot_param.restore(#var);)
            } else {
                let res = &r.ident;
                quote!(#(#cfg)* #res.restore(#var);)
            }
        })
        .collect();
    let alloc_alias = &helpers.alloc_alias;
    let (state_param, state_lease, state_drop) = match state {
        Some((ty, _)) => (
            quote!(, mut __state: #alloc_alias::boxed::Box<#ty>),
            vec![quote!(&mut *__state)],
            // Reclaim the bulk FIRST: before restores, exit-provide, and the
            // completion record, so has_exited() implies the heap is back.
            quote!(::core::mem::drop(__state);),
        ),
        None => (quote!(), vec![], quote!()),
    };
    // `cancel` workers take no node: the shell holds it and races the worker's
    // future against the shutdown signal itself, which is the whole point of the
    // flag — the worker stays a plain async fn with no supervisor in its
    // signature.
    let mut lead: Vec<TokenStream2> = if cancel {
        Vec::new()
    } else {
        vec![quote!(__node)]
    };
    lead.extend(res_leases);
    lead.extend(state_lease);
    let call = inject_call_with(worker, &lead)?;
    // Unsuffixed literal: `#[task]`'s own parser wants a plain integer.
    let ps = LitInt::new(&pool_size.to_string(), proc_macro2::Span::call_site());
    // A diverging (`-> !`) worker makes the trailing statements unreachable —
    // legitimate (a detached/`Pause` worker retains its resources forever), so
    // silence rustc's `unreachable_code` lint on the generated body. Always
    // emitted: the completion record below is an unconditional trailing
    // statement.
    let allow_unreachable = quote!(#[allow(unreachable_code)]);
    // `exit: Type`: bind the worker's return value and provide() it into the
    // node's exit slot BEFORE mark_exited, so has_exited() implies the value is
    // present. A worker whose return type mismatches the declared `exit:` fails
    // at this provide with a plain rustc type error on the shell.
    // Under `cancel` the worker may not have returned at all — the shell holds a
    // `Result<Output, Aborted>` — so the exit value is provided only on a real
    // completion. An aborted worker leaves `<NODE>_EXIT` empty (and
    // `shutdown_requested()` set), which is how a waiter tells "it finished" from
    // "it was stopped".
    //
    // A DIVERGING worker (`-> !`) makes that provide dead code: its future has
    // no output, so the slot could never be filled and every `wait_take()` on
    // it would hang forever. The blanket allow above would hide that, so the
    // provide re-DENIES `unreachable_code` on itself — the one statement in the
    // shell where unreachability is a declaration error rather than a
    // legitimate parked/detached worker. Spanned on the declared `exit:` type,
    // so rustc points at the clause the user has to remove (a bare diverging
    // worker stays legal: that is what `cancel` is for).
    let exit_ident = format_ident!("{}_EXIT", owner);
    let provide = |exit: &syn::Type| {
        // Every token of the statement carries the `exit:` type's span, so the
        // lint's own label lands on that clause instead of the whole item.
        let slot = Ident::new(&exit_ident.to_string(), exit.span());
        quote::quote_spanned!(exit.span()=>
            #[deny(unreachable_code)]
            #slot.provide(__out);
        )
    };
    // The `cancel` arms pin the worker into the shell's own frame and hand
    // `run_cancellable` a `Pin<&mut _>`: the shell then stores the worker's state
    // machine ONCE, whatever rustc decides to do with the callee's arguments
    // (rust-lang/rust#62958 doubles a future passed by value into an `async fn`,
    // and this is static task storage, so the doubling was per node, per binary).
    // The `pin!` lives in its own block so the worker is dropped at the same point
    // it always was — before the restores below, which move the lent resources
    // back out.
    let (drive, provide_exit) = match (cancel, exit) {
        (false, Some(ty)) => {
            let provide = provide(ty);
            (quote!(let __out = #call.await;), provide)
        }
        (false, None) => (quote!(#call.await;), quote!()),
        (true, Some(ty)) => {
            let provide = provide(ty);
            (
                quote!(let __res = { let __fut = ::core::pin::pin!(#call); __node.run_cancellable(__fut).await };),
                quote!(if let ::core::result::Result::Ok(__out) = __res {
                    #provide
                }),
            )
        }
        (true, None) => (
            quote!({
                let __fut = ::core::pin::pin!(#call);
                let _ = __node.run_cancellable(__fut).await;
            }),
            quote!(),
        ),
    };
    let def = quote! {
        #(#cfg)*
        #[::embassy_executor::task(pool_size = #ps)]
        #allow_unreachable
        async fn #shell(__node: &'static #cr::TaskNode #(, #res_params)* #state_param) {
            #drive
            #state_drop
            #(#restores)*
            #provide_exit
            // Record the completion (and ack any pending shutdown handshake):
            // a worker that returns on its own reads as down, not running
            // forever, and a control Activate can respawn it.
            __node.mark_exited();
        }
    };
    let path: Expr = syn::parse_quote!(#shell);
    Ok((def, path))
}

/// Emit a `node`: its `pub static #ident: TaskNode` definition and its `Slot`. The
/// caller assigns the slot index and records the name, so this touches neither.
/// A `task:` node additionally emits its generated shell ahead of the static.
fn emit_node(
    n: &NodeItem,
    cr: &TokenStream2,
    spawn_fn: &TokenStream2,
    // Threaded to ready_tokens: a ready dep naming a pool refs its floor member.
    pool_names: &std::collections::HashSet<String>,
    helpers: &HelperIdents,
) -> SynResult<(TokenStream2, Slot)> {
    let ident = &n.ident;
    let cfg = &n.cfg;
    let mode = &n.mode;
    let name = name_string(&n.ident);
    let disabled = n.disabled;
    let (shell_def, spawn_expr) = match &n.source {
        Some(TaskSource::Shell(worker)) => {
            let ps = match &n.pool_size {
                Some(l) => l.base10_parse::<usize>()?,
                None => 1,
            };
            let (def, path) = emit_shell(
                ident,
                cfg,
                worker,
                ps,
                &n.resources,
                n.exit.as_ref(),
                n.state.as_ref(),
                false,
                n.cancel,
                cr,
                helpers,
            )?;
            (def, Some(path))
        }
        Some(TaskSource::Spawn(e)) => (quote!(), Some(e.clone())),
        None => (quote!(), None),
    };
    let spawn = node_spawn(
        ident,
        &spawn_expr,
        &n.executor,
        &n.resources,
        n.state.as_ref(),
        spawn_fn,
        helpers,
    )?;
    // `executor: NAME` routes the node through that SpawnerSlot; the supervisor
    // awaits the slot before spawning (see `TaskNode::with_executor`).
    let with_exec = match &n.executor {
        Some(ex) => quote!( .with_executor(&#ex) ),
        None => quote!(),
    };
    // `resources: [NAME: Type, ..]` — one `pub static NAME: ResourceSlot<Type>`
    // per entry (main moves the resource in with `NAME.provide(..)`), plus a
    // type-erased gate array wired into the node so the supervisor can await
    // provisioning/restore before each (re)spawn (see `TaskNode::with_resources`).
    // The unsized coercion `&NAME` -> `&dyn ResourceGate` happens in the static
    // initializer, where it is allowed.
    let (res_defs, with_res) = if n.resources.is_empty() {
        (quote!(), quote!())
    } else {
        let gates_ident = format_ident!("__SV_GATES_{}", ident);
        // `shared` slots are emitted once per graph in `expand` (several items
        // may declare the same one); only this node's exclusive (take-kind)
        // slots are emitted here.
        let slot_defs = n.resources.iter().filter(|r| r.shared.is_none()).map(|r| {
            let ecfg = &r.cfg;
            let res = &r.ident;
            let ty = &r.ty;
            // `local` entries use the graph-site slot type (emitted once per
            // graph in `expand`): same provide/take protocol as `ResourceSlot`
            // but without its `T: Send` bound, for `!Send` driver handles on a
            // single-core system. `consume` changes only shell codegen (by-value
            // arg, no restore) — the slot type is the same either way.
            let slot_ty = if r.local.is_some() {
                let local = &helpers.local_slot;
                quote!(#local<#ty>)
            } else {
                quote!(#cr::ResourceSlot<#ty>)
            };
            let doc = if r.consume.is_some() {
                format!(
                    "Resource slot for node `{ident}` (generated by `supervisor_graph!`). \
                         Move the resource in with `.provide(..)` before `Supervisor::start`. \
                         `consume`: the worker owns (and may drop) the value, so the slot is \
                         empty after the task exits — re-`provide()` before any respawn."
                )
            } else {
                format!(
                    "Resource slot for node `{ident}` (generated by `supervisor_graph!`). \
                         Move the resource in with `.provide(..)` before `Supervisor::start`."
                )
            };
            quote! {
                #(#cfg)*
                #(#ecfg)*
                #[doc = #doc]
                pub static #res: #slot_ty = <#slot_ty>::new();
            }
        });
        let (gates_len, gate_refs) = gate_tokens(&n.resources);
        (
            quote! {
                #(#slot_defs)*
                #(#cfg)*
                static #gates_ident: [&'static dyn #cr::ResourceGate; #gates_len] =
                    [#(#gate_refs),*];
            },
            quote!( .with_resources(&#gates_ident) ),
        )
    };
    // `slot_timeout: N` — override the node's pre-spawn slot/gate wait bound
    // (see `TaskNode::with_slot_timeout`; sized to a provider node's build time).
    let with_timeout = match &n.slot_timeout {
        Some(ms) => quote!( .with_slot_timeout(#cr::_export::Duration::from_millis(#ms)) ),
        None => quote!(),
    };
    // `deps: [X ready, ..]` — the ready-marked subset becomes a per-node
    // `[&'static TaskNode; n]` array wired via `.with_ready_deps`: bring-up
    // awaits each one's set_ready() (bounded by slot_timeout) after the
    // resource gates. Spawn-order deps are unaffected (same DEPS table).
    let (ready_def, with_ready) = match ready_tokens(&n.deps, pool_names) {
        Some((len, refs)) => {
            let ready_ident = format_ident!("__SV_READY_{}", ident);
            (
                quote! {
                    #(#cfg)*
                    static #ready_ident: [&'static #cr::TaskNode; #len] = [#(#refs),*];
                },
                quote!( .with_ready_deps(&#ready_ident) ),
            )
        }
        None => (quote!(), quote!()),
    };
    // `exit: Type` — one `pub static <NODE>_EXIT: ResourceSlot<Type>` the shell
    // provide()s the worker's return value into just before mark_exited. Plain
    // `ResourceSlot` on purpose: it is an outbound mailbox, not a gated input,
    // so it joins no gate array (an empty exit slot must not block a spawn).
    let exit_def = match &n.exit {
        Some(ty) => {
            let exit_ident = format_ident!("{}_EXIT", ident);
            let doc = format!(
                "Exit-value slot for node `{ident}` (generated by `supervisor_graph!`). \
                 The generated shell `provide()`s the worker's return value here just \
                 before recording the exit; read it with `.wait_take()` (or `.take()` \
                 after `has_exited()`). Overwritten by the next completion."
            );
            quote! {
                #(#cfg)*
                #[doc = #doc]
                pub static #exit_ident: #cr::ResourceSlot<#ty> =
                    #cr::ResourceSlot::new();
            }
        }
        None => quote!(),
    };
    // Every emitted `pub` item carries a doc string: a consumer crate may be
    // `#![deny(missing_docs)]`, and the lint fires on macro-generated items.
    let node_doc = format!(
        "Supervised node `{ident}` (`{mode}`), generated by `supervisor_graph!`. \
         Pass it to the supervisor's per-node verbs (`start_node`, `stop_node`, \
         `resume_node`, `activate`/`deactivate`); the worker gets the same \
         `&'static TaskNode` for the task-side protocol."
    );
    let def = quote! {
        #res_defs
        #exit_def
        #ready_def
        #shell_def
        #(#cfg)*
        #[doc = #node_doc]
        pub static #ident: #cr::TaskNode =
            #cr::TaskNode::new(#name, #cr::Mode::#mode, #spawn, #disabled)
                #with_exec #with_res #with_timeout #with_ready;
    };
    let slot = Slot {
        cfg_pred: cfg_predicate(cfg),
        reference: quote!(&#ident),
        deps: n.deps.clone(),
        fragment: n.fragment.clone(),
    };
    Ok((def, slot))
}

/// Emit a `pool`: the member `[TaskNode; K]` array, the `spawn_<pool>` glue fn, and
/// the `ElasticPool` static (returned as `defs`, in that emission order), plus the
/// pool-registry entry (for `GRAPH.pools`) and one `Slot` per member (members occupy
/// slots but aren't name-addressable, so no name is recorded).
fn emit_pool(
    p: &PoolItem,
    cr: &TokenStream2,
    spawn_fn: &TokenStream2,
    // Threaded to ready_tokens: a ready dep naming a pool refs its floor member.
    pool_names: &std::collections::HashSet<String>,
    helpers: &HelperIdents,
) -> SynResult<(Vec<TokenStream2>, TokenStream2, Vec<Slot>)> {
    let ident = &p.ident;
    let cfg = &p.cfg;
    let lname = name_string(&p.ident);
    let pool_static = format_ident!("{}_POOL", ident);
    let k = p.modes.len();

    // Validate the scaling bounds. Two paths:
    // - both int literals (the common case): validated HERE, at expansion time,
    //   with the best possible spans. `base10_parse::<u8>` also rejects values
    //   > 255 (the `ElasticPool` fields are `u8`).
    // - otherwise (paths, const exprs — e.g. `min: HTTP_FLOOR`): the emitted
    //   `<POOL>_MIN`/`<POOL>_MAX` consts become the source of truth and
    //   `const _: () = assert!(..)` guards enforce min <= max <= members <= 255
    //   at const-eval time (rendered like the cycle error, with rust-src spans).
    // `min > max` makes the policy contradict itself; `max > k` is a ceiling the
    // pool can never reach (only `k` member slots exist) — declaration bugs
    // either way. `max < k` is allowed (spare declared members below the
    // ceiling), as is `min: 0` (scale to zero when idle). The member count `k`
    // itself stays a structural literal: it drives how many nodes, shells, name
    // strings and graph slots are EMITTED, which a proc macro cannot derive
    // from a const it can't evaluate.
    let lit_bounds = match (&p.min, &p.max) {
        (Expr::Lit(lmin), Expr::Lit(lmax)) => match (&lmin.lit, &lmax.lit) {
            (syn::Lit::Int(imin), syn::Lit::Int(imax)) => {
                Some((imin.base10_parse::<u8>()?, imax.base10_parse::<u8>()?))
            }
            _ => None,
        },
        _ => None,
    };
    if let Some((min_v, max_v)) = lit_bounds {
        if min_v > max_v {
            return Err(syn::Error::new_spanned(
                &p.min,
                format!("pool `min:` ({min_v}) must not exceed `max:` ({max_v})"),
            ));
        }
        if usize::from(max_v) > k {
            return Err(syn::Error::new_spanned(
                &p.max,
                format!("pool `max:` ({max_v}) exceeds the declared member count ({k})"),
            ));
        }
    }

    // Pool `resources:` (all `shared`, enforced at parse) additionally require
    // `task:` — same rule as nodes: the generated shell is what receives the
    // values as arguments (a hand-written `spawn:` task fn manages its own).
    if !p.resources.is_empty() && matches!(p.source, TaskSource::Spawn(_)) {
        return Err(syn::Error::new_spanned(
            &p.resources[0].ident,
            "pool `resources:` requires `task:` — the values are handed to the \
             generated shell as arguments (and lend entries restored by it); a \
             `spawn:` task fn manages its own arguments",
        ));
    }
    if let Some((ty, _)) = &p.state
        && matches!(p.source, TaskSource::Spawn(_))
    {
        return Err(syn::Error::new_spanned(
            ty,
            "pool `state:` requires `task:` — the generated shell owns the boxed \
             state across the worker call; a `spawn:` task fn can Box its own",
        ));
    }

    // Resolve the member task: `spawn:` uses the given expr directly; `task:`
    // first stamps ONE generated shell sized `pool_size = K` (all members share a
    // single concrete future type) and targets that. Shared resources become
    // by-value shell parameters, exactly like a node's.
    let (shell_def, member_expr) = match &p.source {
        TaskSource::Spawn(e) => (quote!(), e.clone()),
        TaskSource::Shell(worker) => emit_shell(
            ident,
            cfg,
            worker,
            k,
            &p.resources,
            None,
            p.state.as_ref(),
            true,
            p.cancel,
            cr,
            helpers,
        )?,
    };
    // Build member `I`'s spawn call from the member task, injecting `&POOL[I]`
    // as the first argument, then the shared resource copies (see
    // `inject_call_with`).
    let res_args: Vec<TokenStream2> = p
        .resources
        .iter()
        .enumerate()
        .flat_map(|(i, r)| {
            let ecfg = &r.cfg;
            let var = format_ident!("__r{}", i);
            let res = &r.ident;
            if r.shared.is_none() && r.consume.is_none() {
                // Lend: value + the member's own slot element, so the shell
                // restores to the same index it was taken from.
                vec![quote!(#(#ecfg)* #var), quote!(#(#ecfg)* &#res[I])]
            } else {
                vec![quote!(#(#ecfg)* #var)]
            }
        })
        .collect();
    let try_box = &helpers.try_box;
    let (state_prelude, state_arg) = match &p.state {
        Some((_, init)) => (
            quote! {
                let __state = #try_box(#init)
                    .ok_or(::embassy_executor::SpawnError::Busy)?;
            },
            vec![quote!(__state)],
        ),
        None => (quote!(), vec![]),
    };
    let mut lead: Vec<TokenStream2> = vec![quote!(&#ident[I])];
    lead.extend(res_args);
    lead.extend(state_arg);
    let call = inject_call_with(&member_expr, &lead)?;
    // Per-member spawn fn: a generated `spawn_<pool>::<I>` wrapper. Same optional
    // trace capture as a node's closure, against member `I`'s slot. With
    // `executor: NAME` the wrapper ignores the supervisor's `Spawner` and spawns
    // through the named SpawnerSlot; each member node carries `.with_executor(&EX)`,
    // so the supervisor awaits the slot (bounded) before invoking the wrapper and
    // the wrapper's `get()` is already filled (`SpawnError::Busy` guards a never-
    // filled slot; member futures must be `Send`). A whole worker pool can thus live
    // on another executor — e.g. the second core — while this core scales it.
    let (param, prelude, sp_tokens) = match &p.executor {
        None => (quote!(s), quote!(), quote!(s)),
        Some(ex) => (
            quote!(_s),
            quote! {
                let __sp = #ex
                    .get()
                    .ok_or(::embassy_executor::SpawnError::Busy)?;
            },
            quote!(__sp),
        ),
    };
    // Resource prelude, kind-aware. `shared`: copy the fan-out handle out
    // non-destructively (slot stays filled for the next member/consumer).
    // Take kinds (lend/consume): take from THIS member's array element —
    // `RES[I]`, per-member exclusive by construction. Either way an unprovided
    // slot fail-closes the member's spawn with `SpawnError::Busy`. After the
    // executor-slot guard, same ordering rationale as a node's glue.
    let get_prelude: Vec<TokenStream2> = p
        .resources
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let ecfg = &r.cfg;
            let res = &r.ident;
            let var = format_ident!("__r{}", i);
            if r.shared.is_some() {
                quote! {
                    #(#ecfg)*
                    let #var = #res
                        .get()
                        .ok_or(::embassy_executor::SpawnError::Busy)?;
                }
            } else {
                quote! {
                    #(#ecfg)*
                    let #var = #res[I]
                        .take()
                        .ok_or(::embassy_executor::SpawnError::Busy)?;
                }
            }
        })
        .collect();
    let pool_spawn_stmts = spawn_stmts(&call, &quote!(&#ident[I]), &sp_tokens);
    let wrapper = format_ident!("spawn_{}", lname);
    let mut defs: Vec<TokenStream2> = Vec::new();
    defs.push(shell_def);
    defs.push(quote! {
        #(#cfg)*
        fn #wrapper<const I: usize>(
            #param: ::embassy_executor::Spawner,
        ) -> ::core::result::Result<(), ::embassy_executor::SpawnError> {
            #prelude
            #state_prelude
            #(#get_prelude)*
            #pool_spawn_stmts
            ::core::result::Result::Ok(())
        }
    });
    let member_spawn: Vec<TokenStream2> = (0..k).map(|j| quote!(#wrapper::<#j>)).collect();

    // `executor: NAME` on the pool routes every member through that SpawnerSlot; the
    // supervisor awaits it before spawning each member (see `TaskNode::with_executor`).
    let member_with_exec = match &p.executor {
        Some(ex) => quote!( .with_executor(&#ex) ),
        None => quote!(),
    };
    // Take-kind entries (lend/consume) get per-member SLOT ARRAYS: member `I`
    // takes/restores index `I` exclusively, so members don't contend and the
    // elastic floor can come up with only floor-many elements provided. The
    // shared slot statics are emitted once per graph in `expand`, as for nodes.
    for r in p.resources.iter().filter(|r| r.shared.is_none()) {
        let ecfg = &r.cfg;
        let res = &r.ident;
        let ty = &r.ty;
        let doc = format!(
            "Per-member resource slots for pool `{ident}` (generated by \
             `supervisor_graph!`): member `I` takes/restores element `I`. \
             Provide at least the floor members' elements before \
             `Supervisor::start`; a member whose element is empty fail-closes \
             its (re)spawn with `SpawnError::Busy`."
        );
        defs.push(quote! {
            #(#cfg)*
            #(#ecfg)*
            #[doc = #doc]
            pub static #res: [#cr::ResourceSlot<#ty>; #k] =
                [const { #cr::ResourceSlot::new() }; #k];
        });
    }
    // Per-member gate arrays: member `j` gates on ITS OWN take-kind elements
    // plus the pool-wide shared slots. Same cfg-aware length for every member.
    let member_with_res: Vec<TokenStream2> = if p.resources.is_empty() {
        (0..k).map(|_| quote!()).collect()
    } else {
        let any_cfg = p.resources.iter().any(|r| cfg_predicate(&r.cfg).is_some());
        let gates_len = if any_cfg {
            let terms: Vec<TokenStream2> = p
                .resources
                .iter()
                .map(|r| match cfg_predicate(&r.cfg) {
                    None => quote!(1usize),
                    Some(pred) => quote!({
                        #[cfg(#pred)]
                        {
                            1usize
                        }
                        #[cfg(not(#pred))]
                        {
                            0usize
                        }
                    }),
                })
                .collect();
            quote!(0usize #(+ #terms)*)
        } else {
            let n = p.resources.len();
            quote!(#n)
        };
        (0..k)
            .map(|j| {
                let gates_ident = format_ident!("__SV_GATES_{}_{}", ident, j);
                let gate_refs: Vec<TokenStream2> = p
                    .resources
                    .iter()
                    .map(|r| {
                        let ecfg = &r.cfg;
                        let res = &r.ident;
                        if r.shared.is_some() {
                            quote!(#(#ecfg)* &#res)
                        } else {
                            quote!(#(#ecfg)* &#res[#j])
                        }
                    })
                    .collect();
                defs.push(quote! {
                    #(#cfg)*
                    static #gates_ident: [&'static dyn #cr::ResourceGate; #gates_len] =
                        [#(#gate_refs),*];
                });
                quote!( .with_resources(&#gates_ident) )
            })
            .collect()
    };
    // `deps: [X ready, ..]` — ONE shared ready-dep array for the whole pool
    // (markers apply to every member; growth also checks it synchronously).
    let member_with_ready = match ready_tokens(&p.deps, pool_names) {
        Some((len, refs)) => {
            let ready_ident = format_ident!("__SV_READY_{}", ident);
            defs.push(quote! {
                #(#cfg)*
                static #ready_ident: [&'static #cr::TaskNode; #len] = [#(#refs),*];
            });
            quote!( .with_ready_deps(&#ready_ident) )
        }
        None => quote!(),
    };
    // `slot_timeout: N` — every member's pre-spawn slot/gate wait bound.
    let member_with_timeout = match &p.slot_timeout {
        Some(ms) => quote!( .with_slot_timeout(#cr::_export::Duration::from_millis(#ms)) ),
        None => quote!(),
    };
    let members = p
        .modes
        .iter()
        .zip(&member_spawn)
        .enumerate()
        .map(|(j, (mode, sp))| {
            let nm = format!("{lname}{j}");
            let with_res = &member_with_res[j];
            quote! {
                #cr::TaskNode::new(
                    #nm, #cr::Mode::#mode,
                    ::core::option::Option::Some((#sp) as #spawn_fn), false,
                ) #member_with_exec #with_res #member_with_timeout #member_with_ready
            }
        });
    defs.push(quote! {
        #(#cfg)*
        #[doc = concat!("Pool `", stringify!(#ident), "`'s members, one `TaskNode` per slot \
            (index = member index). Index it for the per-node verbs; the pool itself is \
            `", stringify!(#ident), "_POOL`.")]
        pub static #ident: [#cr::TaskNode; #k] = [ #(#members),* ];
    });

    // Structural constants, for downstream compile-time sizing (e.g. a socket
    // budget: `const BUDGET: usize = HTTP_MAX + 1`). Emitted because user code
    // can't derive them from the member array — a `const` cannot refer to a
    // `static` (E0013), so `HTTP.len()` is unusable in const context and the
    // count would otherwise have to be duplicated by hand next to the DSL.
    let min_const = format_ident!("{}_MIN", ident);
    let max_const = format_ident!("{}_MAX", ident);
    let members_const = format_ident!("{}_MEMBERS", ident);
    // Literal path: emit the *validated* u8 values. Expr path: the consts ARE
    // the source of truth (any const-evaluable usize expr) and const asserts
    // enforce what the literal path checked at expansion.
    let (min_tokens, max_tokens, bound_asserts) = match lit_bounds {
        Some((min_v, max_v)) => {
            let (min_u, max_u) = (usize::from(min_v), usize::from(max_v));
            (quote!(#min_u), quote!(#max_u), quote!())
        }
        None => {
            let (min_e, max_e) = (&p.min, &p.max);
            (
                quote!({ #min_e }),
                quote!({ #max_e }),
                quote! {
                    #(#cfg)*
                    const _: () = ::core::assert!(
                        #min_const <= #max_const,
                        "pool `min:` must not exceed `max:`",
                    );
                    #(#cfg)*
                    const _: () = ::core::assert!(
                        #max_const <= #members_const,
                        "pool `max:` exceeds the declared member count",
                    );
                    #(#cfg)*
                    const _: () = ::core::assert!(
                        #max_const <= 255,
                        "pool `max:` exceeds 255 (ElasticPool bounds are u8)",
                    );
                },
            )
        }
    };
    defs.push(quote! {
        #(#cfg)*
        #[doc = concat!("Pool `", stringify!(#ident), "`'s `min:` floor (validated at expansion or by const assert).")]
        pub const #min_const: usize = #min_tokens;
        #(#cfg)*
        #[doc = concat!("Pool `", stringify!(#ident), "`'s `max:` scaling ceiling — the most members ever running concurrently.")]
        pub const #max_const: usize = #max_tokens;
        #(#cfg)*
        #[doc = concat!("Pool `", stringify!(#ident), "`'s declared member count (the `[TaskNode; K]` array length).")]
        pub const #members_const: usize = #k;
        #bound_asserts
    });

    let member_refs = (0..k).map(|j| quote!(&#ident[#j]));
    let policy = &p.policy;
    // The `ElasticPool<P>` type argument: honor an explicit `policy: <Ty> = ..`
    // annotation, else derive `P` from the constructor expr (`Ty::new(..)` shape).
    let policy_ty = match &p.policy_ty {
        Some(ty) => quote!(#ty),
        None => {
            let path = policy_type(policy)?;
            quote!(#path)
        }
    };
    // The u8 fields come from the emitted consts, which both paths validate
    // (parse-time for literals, const asserts otherwise) — so the `as u8` casts
    // cannot truncate. Going through the consts also keeps a suffixed literal
    // like `min: 3usize` working (the const is usize either way).
    defs.push(quote! {
        #(#cfg)*
        #[doc = concat!("The `ElasticPool` over the `", stringify!(#ident), "` members: \
            the `min:`/`max:` bounds and the scaling policy `Supervisor::run_pools` \
            drives. Also reachable through `GRAPH.pools`.")]
        pub static #pool_static: #cr::ElasticPool<#policy_ty> = #cr::ElasticPool {
            nodes: &[ #(#member_refs),* ],
            min: #min_const as u8,
            max: #max_const as u8,
            policy: #policy,
        };
    });

    let pool_entry = quote!( #(#cfg)* &#pool_static );

    let pred = cfg_predicate(cfg);
    let slots = (0..k)
        .map(|j| Slot {
            cfg_pred: pred.clone(),
            reference: quote!(&#ident[#j]),
            deps: p.deps.clone(),
            fragment: p.fragment.clone(),
        })
        .collect();

    Ok((defs, pool_entry, slots))
}

/// Second pass: build the node-slot entries for `GRAPH.nodes` (`Option`, cfg-gated) and
/// the cfg-aware dep-index entries for `GRAPH.deps`. Runs after every slot + name is
/// known, since a dep may forward-reference a node declared later. An unknown dep name
/// is a compile error.
fn slot_tables(
    slots: &[Slot],
    names: &HashMap<String, usize>,
) -> SynResult<(Vec<TokenStream2>, Vec<TokenStream2>)> {
    let mut all_entries: Vec<TokenStream2> = Vec::new();
    let mut deps_entries: Vec<TokenStream2> = Vec::new();
    for slot in slots {
        let reference = &slot.reference;
        all_entries.push(match &slot.cfg_pred {
            None => quote!(::core::option::Option::Some(#reference)),
            Some(pred) => quote!({
                #[cfg(#pred)]
                { ::core::option::Option::Some(#reference) }
                #[cfg(not(#pred))]
                { ::core::option::Option::None }
            }),
        });

        let mut dep_toks: Vec<TokenStream2> = Vec::new();
        // Duplicate deps are a compile error: `deps: [A, A]` would emit a doubled
        // index, which `topo_sort_const` counts twice in the in-degree but decrements
        // once — misreported as a dependency cycle. Compared by *resolved* slot index
        // (so a repeated pool name trips it too); two cfg-gated variants of the same
        // dep are allowed only when their cfg predicates differ.
        let mut seen: Vec<(u8, String)> = Vec::new();
        for d in &slot.deps {
            let idx = match names.get(&d.ident.to_string()) {
                Some(&i) => i as u8,
                None => {
                    return Err(syn::Error::new_spanned(
                        &d.ident,
                        format!(
                            "unknown dependency `{}` — not a declared node or pool{}",
                            d.ident,
                            fragment_suffix(&slot.fragment),
                        ),
                    ));
                }
            };
            let cfg = &d.cfg;
            let cfg_key = quote!( #(#cfg)* ).to_string();
            if seen.iter().any(|(i, k)| *i == idx && *k == cfg_key) {
                return Err(syn::Error::new_spanned(
                    &d.ident,
                    format!("duplicate dependency `{}`", d.ident),
                ));
            }
            seen.push((idx, cfg_key));
            dep_toks.push(quote!( #(#cfg)* #idx ));
        }
        deps_entries.push(quote!( &[ #(#dep_toks),* ] ));
    }
    Ok((all_entries, deps_entries))
}

fn expand(graph: GraphSpec) -> SynResult<TokenStream2> {
    let cr = quote!(::embassy_supervisor);
    let helpers = HelperIdents::new(graph.name.as_ref());
    // The node spawn fn-pointer type. Spawn exprs (closures / const-generic fns) are
    // cast to this so they coerce cleanly inside `Option::Some(..)`.
    let spawn_fn = quote!(
        fn(
            ::embassy_executor::Spawner,
        ) -> ::core::result::Result<(), ::embassy_executor::SpawnError>
    );

    // First pass: emit the statics/glue in declaration order, assign stable slot
    // indices, and record each slot + its raw deps. `names` maps a dep-addressable ident
    // to its slot index for dep resolution — keyed on the *raw* ident (not the runtime
    // `name_string`). A `node` maps to its own slot; a `pool` maps to its floor member's
    // slot (so `deps: [POOL]` = "after the pool is up"). Individual pool members are not
    // separately name-addressable.
    let mut defs: Vec<TokenStream2> = Vec::new();
    let mut pool_entries: Vec<TokenStream2> = Vec::new();
    let mut slots: Vec<Slot> = Vec::new();
    let mut names: HashMap<String, usize> = HashMap::new();

    // Iff any `resources:` entry is `local`-marked, emit the local slot TYPE once
    // per graph (the per-entry statics in `emit_node` reference it by name). It
    // mirrors `embassy_supervisor::ResourceSlot` — same provide/take/restore
    // protocol, same critical-section interior, same `ResourceGate` view — but
    // WITHOUT the `T: Send` bound, so it can carry the `!Send` driver handles
    // (`RefCell`-/`NoopRawMutex`-based: `embassy_net::Stack` runners,
    // `cyw43::Control`, …) that a single-core system hands between its own tasks.
    // That requires asserting `Sync` for a `!Send` payload, so like the
    // `trace-hooks` symbols it is emitted here, at the graph declaration site,
    // where the application owns the soundness contract (see the SAFETY note).
    // `state:` anywhere in the graph: emit the fallible-boxing helper ONCE, at
    // the graph site (like the local slot type). This is the `heap-state`
    // feature's ENTIRE unsafe surface, and it lives in the CONSUMER crate (the
    // `local-resources` precedent): raw alloc + null check + ptr::write +
    // Box::from_raw — after which it is a NORMAL Box, freed by ordinary drop
    // when the shell drops it on task exit. Alloc failure returns None (the
    // glue maps it to SpawnError::Busy; the init value is dropped normally).
    let any_state = graph.items.iter().any(|item| match item {
        Item::Node(n) => n.state.is_some(),
        Item::Pool(p) => p.state.is_some(),
        Item::Executor(_) => false,
    });
    if any_state {
        let try_box = &helpers.try_box;
        let alloc_alias = &helpers.alloc_alias;
        defs.push(quote! {
            extern crate alloc as #alloc_alias;
            /// Fallible boxing for `state:` clauses (generated by
            /// `supervisor_graph!`). Returns `None` when the global allocator
            /// is out of memory — surfaced by the spawn glue as
            /// `SpawnError::Busy`, retryable once heap frees up. The value is
            /// written to the heap allocation directly; note the INIT argument
            /// itself is materialized in this call's frame first (rustc may or
            /// may not elide the copy) — keep `state:` types reasonably sized
            /// or box internal bulk.
            #[doc(hidden)]
            fn #try_box<T>(init: T) -> ::core::option::Option<#alloc_alias::boxed::Box<T>> {
                let layout = ::core::alloc::Layout::new::<T>();
                if layout.size() == 0 {
                    // ZST: no allocation. Box<ZST> from a dangling well-aligned
                    // pointer is the documented representation; `init` is
                    // forgotten so T's drop (if any) runs exactly once, via the
                    // Box.
                    ::core::mem::forget(init);
                    // SAFETY: dangling NonNull is valid for a ZST Box.
                    return ::core::option::Option::Some(unsafe {
                        #alloc_alias::boxed::Box::from_raw(
                            ::core::ptr::NonNull::<T>::dangling().as_ptr(),
                        )
                    });
                }
                // SAFETY: `layout` has non-zero size. On success the pointer is
                // valid for writes of `T` and exclusively ours; `write`
                // initializes it; `from_raw` then owns an allocation made with
                // the global allocator and `T`'s layout — a normal `Box`.
                unsafe {
                    let p = #alloc_alias::alloc::alloc(layout) as *mut T;
                    if p.is_null() {
                        return ::core::option::Option::None; // `init` drops here
                    }
                    ::core::ptr::write(p, init);
                    ::core::option::Option::Some(#alloc_alias::boxed::Box::from_raw(p))
                }
            }
        });
    }

    let any_local = graph
        .items
        .iter()
        .any(|item| item_resources(item).iter().any(|r| r.local.is_some()));
    if any_local {
        let local = helpers.local_slot.clone();
        // `Cell<Option<T>>` spelled through absolute paths (macro output must not
        // rely on the caller's prelude/imports); the mutex/signal types come from
        // the supervisor's `_export` shim so the consumer needs no direct
        // `embassy-sync` dependency.
        let cell = quote!(::core::cell::Cell<::core::option::Option<T>>);
        let raw = quote!(#cr::_export::CriticalSectionRawMutex);
        let signal = quote!(#cr::_export::Signal<#raw, ()>);
        defs.push(quote! {
            /// One-value handoff cell for a `local`-marked `resources:` entry
            /// (generated by `supervisor_graph!`). Protocol and fail-closed
            /// semantics of `embassy_supervisor::ResourceSlot`, minus its
            /// `T: Send` bound — for `!Send` driver handles on a single core.
            ///
            /// Contract (see the `unsafe impl Sync` below): every `provide` /
            /// `take` / `restore` of a given slot must happen on the SAME core.
            // `dead_code`/`missing_docs` in the consumer: the type is emitted
            // whenever a `local` entry is *declared*, even if every declaring
            // node is `#[cfg]`-compiled out of this build.
            #[allow(dead_code)]
            pub struct #local<T> {
                slot: #cr::_export::BlockingMutex<#raw, #cell>,
                filled: #signal,
            }
            // SAFETY: the payload is intentionally NOT `Send` — this assertion is
            // exactly the single-core contract: the value only ever moves between
            // executors/tasks of one core (interrupt-safe via the critical-section
            // mutex around every access), never across cores. The macro rejects
            // `local` + `executor:` so a slot cannot feed a `SendSpawner`-routed
            // node, and a multi-core application must not `provide`/`take` a given
            // slot from different cores.
            unsafe impl<T> ::core::marker::Sync for #local<T> {}
            #[allow(dead_code)]
            impl<T> #local<T> {
                /// An empty slot (`const` — it lives in the generated `static`s).
                pub const fn new() -> Self {
                    Self {
                        slot: #cr::_export::BlockingMutex::new(
                            ::core::cell::Cell::new(::core::option::Option::None),
                        ),
                        filled: #cr::_export::Signal::new(),
                    }
                }
                /// Move the resource in and wake the supervisor's pre-spawn wait.
                pub fn provide(&self, value: T) {
                    self.slot.lock(|c| c.set(::core::option::Option::Some(value)));
                    self.filled.signal(());
                }
                /// Take the resource out, leaving the slot empty (spawn glue).
                pub fn take(&self) -> ::core::option::Option<T> {
                    self.slot.lock(::core::cell::Cell::take)
                }
                /// Put the resource back for the next spawn (generated shell;
                /// not emitted for `consume` entries).
                pub fn restore(&self, value: T) {
                    self.provide(value);
                }
            }
            #[allow(dead_code)]
            impl<T: ::core::marker::Copy> #local<T> {
                /// Copy the value out WITHOUT emptying the slot — the `shared`
                /// kind's fan-out read (any number of consumers, slot stays
                /// filled). `T: Copy` only.
                pub fn get(&self) -> ::core::option::Option<T> {
                    self.slot.lock(|c| {
                        let v = c.take();
                        c.set(v);
                        v
                    })
                }
            }
            impl<T> ::core::default::Default for #local<T> {
                fn default() -> Self {
                    Self::new()
                }
            }
            impl<T> #cr::ResourceGate for #local<T> {
                fn is_filled(&self) -> bool {
                    // Peek without consuming: `Cell` has no `&T` access, so
                    // take-and-put-back under the same critical section.
                    self.slot.lock(|c| {
                        let v = c.take();
                        let filled = v.is_some();
                        c.set(v);
                        filled
                    })
                }
                fn filled_signal(&self) -> &#signal {
                    &self.filled
                }
            }
        });
    }

    // Pre-pass: collect the declared `executor NAME;` slots so a node's
    // `executor:` reference can be validated regardless of declaration order.
    let helpers = HelperIdents::new(graph.name.as_ref());
    let executor_names: Vec<String> = graph
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Executor(x) => Some(x.ident.to_string()),
            _ => None,
        })
        .collect();
    // Pool idents, known up front: a `ready`-marked dep naming a pool resolves
    // to the pool's floor member (`&POOL[0]`), and forward references are legal.
    let pool_names: std::collections::HashSet<String> = graph
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Pool(p) => Some(p.ident.to_string()),
            _ => None,
        })
        .collect();

    // Pre-pass: `resources:` slot names become `pub static`s at the declaration
    // site, so take-kind names must be unique across the whole graph — and no
    // resource may shadow an `executor NAME;` static. `shared` entries are the
    // deliberate exception: the SAME name on several items is one fan-out slot,
    // emitted once (below, with the union of the declaring sites' cfg
    // predicates so it exists whenever any consumer does) — provided every
    // re-declaration repeats the kinds + type verbatim. Caught here with
    // targeted messages instead of rustc's downstream duplicate-static E0428.
    struct SharedPlan<'a> {
        /// First declaration — supplies the emitted static's ident (span), type,
        /// and `local` flag.
        decl: &'a ResourceDecl,
        /// Kinds+type token string every re-declaration must match.
        sig: String,
        /// One entry per declaring site: `None` = unconditional (the slot is
        /// then unconditional too), `Some(pred)` = that site's combined
        /// item-level + entry-level cfg predicate.
        preds: Vec<Option<TokenStream2>>,
        /// Declaring node/pool names, for the generated doc comment.
        owners: Vec<String>,
    }
    let mut shared_plans: Vec<(String, SharedPlan)> = Vec::new();
    {
        let mut taken: HashSet<String> = HashSet::new();
        for item in &graph.items {
            let Some((owner, item_cfg)) = item_ident_cfg(item) else {
                continue;
            };
            let item_pred = cfg_predicate(item_cfg);
            for r in item_resources(item) {
                let key = r.ident.to_string();
                if executor_names.contains(&key) {
                    return Err(syn::Error::new_spanned(
                        &r.ident,
                        format!(
                            "resource name `{}` shadows an `executor {};` slot — \
                             both are statics at the declaration site",
                            r.ident, r.ident
                        ),
                    ));
                }
                // A site's presence predicate: the item's cfg AND the entry's.
                let pred = match (item_pred.clone(), cfg_predicate(&r.cfg)) {
                    (None, None) => None,
                    (Some(p), None) | (None, Some(p)) => Some(p),
                    (Some(a), Some(b)) => Some(quote!(all(#a, #b))),
                };
                if r.shared.is_some() {
                    if taken.contains(&key) {
                        return Err(syn::Error::new_spanned(
                            &r.ident,
                            format!(
                                "`{}` is already a take-kind resource elsewhere in \
                                 the graph — a name is either one exclusive slot or \
                                 one `shared` slot, not both",
                                r.ident
                            ),
                        ));
                    }
                    let sig = r.shared_signature();
                    match shared_plans.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, plan)) => {
                            if plan.sig != sig {
                                return Err(syn::Error::new_spanned(
                                    &r.ident,
                                    format!(
                                        "shared resource `{}` re-declared with a \
                                         different shape: `{}` here vs `{}` on \
                                         `{}` — every declaration of a shared slot \
                                         must repeat the same kind markers and type",
                                        r.ident, sig, plan.sig, plan.owners[0]
                                    ),
                                ));
                            }
                            plan.preds.push(pred);
                            plan.owners.push(owner.to_string());
                        }
                        None => shared_plans.push((
                            key,
                            SharedPlan {
                                decl: r,
                                sig,
                                preds: vec![pred],
                                owners: vec![owner.to_string()],
                            },
                        )),
                    }
                } else {
                    if !taken.insert(key.clone()) || shared_plans.iter().any(|(k, _)| *k == key) {
                        return Err(syn::Error::new_spanned(
                            &r.ident,
                            format!(
                                "duplicate resource name `{}` — resource slots are \
                                 statics and must be unique across the graph (only \
                                 `shared` entries may repeat a name)",
                                r.ident
                            ),
                        ));
                    }
                }
            }
        }
    }
    // Emit each shared slot once. Presence: unconditional if ANY declaring site
    // is, else `#[cfg(any(<site preds>))]` — the slot exists whenever at least
    // one consumer does.
    for (_, plan) in &shared_plans {
        let res = &plan.decl.ident;
        let ty = &plan.decl.ty;
        let slot_ty = if plan.decl.local.is_some() {
            let local = &helpers.local_slot;
            quote!(#local<#ty>)
        } else {
            quote!(#cr::ResourceSlot<#ty>)
        };
        let cfg_attr = if plan.preds.iter().any(|p| p.is_none()) {
            quote!()
        } else {
            let preds = plan.preds.iter().flatten();
            quote!(#[cfg(any(#(#preds),*))])
        };
        let doc = format!(
            "Shared (fan-out) resource slot declared by `{}` (generated by \
             `supervisor_graph!`). `provide()` the `Copy` handle before \
             `Supervisor::start`; every consumer's glue copies it out with \
             `get()`, so the slot STAYS FILLED — re-`provide()` only to replace \
             the handle (e.g. after rebuilding the underlying driver).",
            plan.owners.join("`, `"),
        );
        defs.push(quote! {
            #cfg_attr
            #[doc = #doc]
            pub static #res: #slot_ty = <#slot_ty>::new();
        });
    }

    for item in &graph.items {
        match item {
            Item::Node(n) => {
                if let Some(ex) = &n.executor
                    && !executor_names.contains(&ex.to_string())
                {
                    return Err(syn::Error::new_spanned(
                        ex,
                        format!(
                            "unknown executor `{ex}`; declare it in the graph with \
                             `executor {ex};` (declared: [{}])",
                            executor_names.join(", ")
                        ),
                    ));
                }
                // The index is the slot's position, taken *before* the push.
                // A redeclared name is a hard error here (not just the downstream
                // `duplicate definition of static`): deps resolve through this map,
                // so a silent overwrite would silently rewire earlier `deps:` edges.
                if names.insert(n.ident.to_string(), slots.len()).is_some() {
                    return Err(syn::Error::new_spanned(
                        &n.ident,
                        format!(
                            "duplicate node/pool name `{}`{}",
                            n.ident,
                            fragment_suffix(&n.fragment),
                        ),
                    ));
                }
                let (def, slot) = emit_node(n, &cr, &spawn_fn, &pool_names, &helpers)?;
                defs.push(def);
                slots.push(slot);
            }
            Item::Executor(x) => {
                let (cfg, ident) = (&x.cfg, &x.ident);
                // A runtime-filled SendSpawner slot: the app registers the
                // executor's spawner before `Supervisor::start`; nodes declared
                // `executor: NAME` spawn through it. Occupies no graph slot.
                defs.push(quote! {
                    #(#cfg)*
                    /// Spawner slot for the graph's `executor:`-annotated nodes
                    /// (generated by `supervisor_graph!`). Fill with
                    /// `SpawnerSlot::set` before `Supervisor::start`.
                    pub static #ident: #cr::SpawnerSlot = #cr::SpawnerSlot::new();
                });
            }
            Item::Pool(p) => {
                // Pools are only meaningful with the supervisor's `pool` feature (which
                // forwards to this crate). Without it, `Graph` has no `pools` field and
                // `ElasticPool` doesn't exist — so refuse a `pool` with a clear message
                // rather than emitting dangling references.
                if cfg!(feature = "pool") {
                    if let Some(ex) = &p.executor
                        && !executor_names.contains(&ex.to_string())
                    {
                        return Err(syn::Error::new_spanned(
                            ex,
                            format!(
                                "unknown executor `{ex}`; declare it in the graph with \
                                 `executor {ex};` (declared: [{}])",
                                executor_names.join(", ")
                            ),
                        ));
                    }
                    let (pool_defs, pool_entry, pool_slots) =
                        emit_pool(p, &cr, &spawn_fn, &pool_names, &helpers)?;
                    // A dep on the pool NAME resolves to the pool's floor member (member 0
                    // — the `min`-kept, always-started member): `deps: [POOL]` means "after
                    // the pool is up". `slots.len()` here is that member's slot index, taken
                    // *before* the extend below (pool_slots[0] lands at exactly this index).
                    // A redeclared name errors, same as the node arm.
                    if names.insert(p.ident.to_string(), slots.len()).is_some() {
                        return Err(syn::Error::new_spanned(
                            &p.ident,
                            format!(
                                "duplicate node/pool name `{}`{}",
                                p.ident,
                                fragment_suffix(&p.fragment),
                            ),
                        ));
                    }
                    defs.extend(pool_defs);
                    pool_entries.push(pool_entry);
                    slots.extend(pool_slots);
                } else {
                    return Err(syn::Error::new_spanned(
                        &p.ident,
                        "a `pool` requires enabling embassy-supervisor's `pool` feature",
                    ));
                }
            }
        }
    }

    let m = slots.len();
    // Every graph index (dep entries, `topo_sort_const`'s queue/order) is a `u8`, so
    // more than 256 slots would silently truncate (`i as u8`) and corrupt the order.
    // 256 slots means max index 255 and max per-node dep count 255 — both fit exactly.
    if m > 256 {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "supervisor_graph!: {m} node slots declared, but at most 256 are supported \
                 (including pool members) — graph indices are `u8`"
            ),
        ));
    }
    let (all_entries, deps_entries) = slot_tables(&slots, &names)?;

    // `Graph.pools` is `#[cfg(feature = "pool")]`; emit that field iff this macro was
    // built with pool support (forwarded from the supervisor's `pool` feature).
    let pools_field = if cfg!(feature = "pool") {
        quote!( pools: &[ #(#pool_entries),* ], )
    } else {
        quote!()
    };

    // embassy-executor's trace hooks (declared `unsafe extern "Rust"` in the
    // executor), defined once here at the graph declaration site — the supervisor
    // crate is `forbid(unsafe_code)` and cannot carry `#[unsafe(no_mangle)]` items.
    // They forward to the supervisor's `trace` recorders. `task_new` and
    // `task_ready_begin` carry nothing the recorders need (the id→node mapping
    // comes from the spawn glue above), so they are no-ops. Exactly one definition
    // of each may exist per binary: enable `trace-hooks` OR write your own set.
    // Requires an edition-2024 consumer (`#[unsafe(no_mangle)]` syntax).
    // Named graphs never emit the hook symbols: `no_mangle` items exist once
    // per binary, and a multi-graph binary's PRIMARY (unnamed) graph carries
    // them; the recorders resolve every registered graph's nodes regardless.
    let trace_hooks = if cfg!(feature = "trace-hooks") && graph.name.is_none() {
        quote! {
            #[unsafe(no_mangle)]
            fn _embassy_trace_poll_start(executor_id: u32) {
                #cr::trace::on_poll_start(executor_id);
            }
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_new(_executor_id: u32, _task_id: u32) {}
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_end(executor_id: u32, task_id: u32) {
                #cr::trace::on_task_end(executor_id, task_id);
            }
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_exec_begin(executor_id: u32, task_id: u32) {
                #cr::trace::on_task_exec_begin(executor_id, task_id);
            }
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_exec_end(executor_id: u32, task_id: u32) {
                #cr::trace::on_task_exec_end(executor_id, task_id);
            }
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_ready_begin(_executor_id: u32, _task_id: u32) {}
            #[unsafe(no_mangle)]
            fn _embassy_trace_executor_idle(executor_id: u32) {
                #cr::trace::on_executor_idle(executor_id);
            }
        }
    } else {
        quote!()
    };

    // `name: X;` renames the emitted graph static and suffixes the private
    // backing tables, so several graphs coexist — even in one module. Unnamed
    // keeps the historical `GRAPH`/`NODES`/`DEPS` idents.
    let graph_ident = graph
        .name
        .clone()
        .unwrap_or_else(|| Ident::new("GRAPH", proc_macro2::Span::call_site()));
    let (nodes_ident, deps_ident) = match &graph.name {
        Some(n) => (
            format_ident!("__SV_NODES_{}", n),
            format_ident!("__SV_DEPS_{}", n),
        ),
        None => (
            Ident::new("NODES", proc_macro2::Span::call_site()),
            Ident::new("DEPS", proc_macro2::Span::call_site()),
        ),
    };
    Ok(quote! {
        #(#defs)*

        // Private backing tables — the application uses the graph static. The
        // topological order and pools are inlined into its literal below; the
        // node count is `.nodes.len()`.
        static #nodes_ident: [::core::option::Option<&'static #cr::TaskNode>; #m] = [ #(#all_entries),* ];
        const #deps_ident: [&'static [u8]; #m] = [ #(#deps_entries),* ];

        /// The compile-time task graph — node slots, dependency table, topological order,
        /// and (with the `pool` feature) the elastic pools. Pass to `Supervisor::new`.
        pub static #graph_ident: #cr::Graph<#m> = #cr::Graph {
            nodes: &#nodes_ident,
            deps: &#deps_ident,
            order: #cr::topo_sort_const(&#deps_ident),
            #pools_field
        };

        #trace_hooks
    })
}

/// Declare a supervised task graph; see the crate docs for the surface syntax.
#[proc_macro]
pub fn supervisor_graph(input: TokenStream) -> TokenStream {
    let graph = syn::parse_macro_input!(input as GraphSpec);
    expand(graph)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Declare a **graph fragment**: `supervisor_fragment! { name: NET_FRAG; <items> }`
/// emits a `#[macro_export] macro_rules! NET_FRAG` relay that forwards the items
/// (verbatim, wrapped in `@fragment`/`@endfragment` attribution markers) into the
/// single `supervisor_graph!` expansion a `compose_graph!` call site assembles —
/// so every whole-graph compile-time pass (name map, u8 slot indices, topo order,
/// shared-slot dedup, the 256 cap) still sees ALL items, across crates.
///
/// Item syntax is validated here, with fragment-site spans; dep/executor NAMES
/// resolve at the compose site (cross-fragment references are the point).
/// Fragment authors reference their own workers/types via `$crate::…` (which
/// hygienically resolves to the fragment's crate at every compose site) or a
/// fully-qualified `::crate_name::…` path; a bare `crate::…` would resolve at
/// the COMPOSE crate and is a bug. No `$` other than `$crate` is permitted.
/// `#[cfg(...)]` inside a fragment is evaluated against the COMPOSE crate's
/// features (the tokens expand there) — export differently-named fragment
/// variants instead of feature-gating items.
#[proc_macro]
pub fn supervisor_fragment(input: TokenStream) -> TokenStream {
    fragment_expand(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn fragment_expand(input: TokenStream2) -> SynResult<TokenStream2> {
    struct FragmentSpec {
        name: Ident,
        items: TokenStream2,
    }
    impl Parse for FragmentSpec {
        fn parse(input: ParseStream) -> SynResult<Self> {
            input.parse::<kw::name>()?;
            input.parse::<Token![:]>()?;
            let name: Ident = input.parse()?;
            input.parse::<Token![;]>()?;
            let items: TokenStream2 = input.parse()?;
            Ok(FragmentSpec { name, items })
        }
    }
    let spec: FragmentSpec = syn::parse2(input)?;
    let name = &spec.name;

    // Only `$crate` may appear (it resolves to the fragment's own crate in the
    // emitted macro_rules RHS); any other `$` would be interpreted as a
    // metavariable by the relay and mangle the forwarded tokens.
    validate_dollars(spec.items.clone())?;

    // Syntax validation with fragment-site spans: parse the items as a graph,
    // with `$crate` substituted by a placeholder ident so paths parse. Name
    // RESOLUTION (deps, executors) is deliberately skipped — targets may live
    // in other fragments and resolve at the compose site.
    let substituted = substitute_dollar_crate(spec.items.clone());
    syn::parse2::<GraphSpec>(substituted)?;

    let items = &spec.items;
    let dollar = proc_macro2::Punct::new('$', proc_macro2::Spacing::Alone);
    let doc = format!(
        "A `supervisor_fragment!` relay (generated). Use from a compose site:\n\
         `embassy_supervisor::compose_graph! {{ fragments: [{name}], graph: {{ .. }} }}`\n\
         Not for direct invocation."
    );
    Ok(quote! {
        #[doc = #doc]
        #[macro_export]
        macro_rules! #name {
            (@emit #dollar cb:path, [#dollar(#dollar rest:tt)*], {#dollar(#dollar acc:tt)*}, {#dollar(#dollar g:tt)*}) => {
                #dollar cb! { @next [#dollar(#dollar rest)*],
                    {#dollar(#dollar acc)* @fragment #name; #items @endfragment;},
                    {#dollar(#dollar g)*} }
            };
        }
    })
}

/// Reject any `$` not immediately followed by `crate`, recursively through
/// groups. `$crate` is the one dollar token with meaning in the emitted
/// macro_rules RHS (fragment-crate paths); anything else would be read as a
/// metavariable.
fn validate_dollars(stream: TokenStream2) -> SynResult<()> {
    use proc_macro2::TokenTree;
    let mut iter = stream.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            TokenTree::Group(g) => validate_dollars(g.stream())?,
            TokenTree::Punct(p) if p.as_char() == '$' => match iter.peek() {
                Some(TokenTree::Ident(i)) if i == "crate" => {}
                _ => {
                    return Err(syn::Error::new(
                        p.span(),
                        "only `$crate` is permitted in a fragment — any other `$` \
                         would be read as a metavariable by the relay macro",
                    ));
                }
            },
            _ => {}
        }
    }
    Ok(())
}

/// Replace every `$crate` pair with a placeholder ident so the items parse as a
/// `GraphSpec` for validation. The ORIGINAL tokens (with `$crate` intact) are
/// what get forwarded.
fn substitute_dollar_crate(stream: TokenStream2) -> TokenStream2 {
    use proc_macro2::{TokenStream as TS, TokenTree};
    let mut out = TS::new();
    let mut iter = stream.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            TokenTree::Group(g) => {
                let inner = substitute_dollar_crate(g.stream());
                let mut ng = proc_macro2::Group::new(g.delimiter(), inner);
                ng.set_span(g.span());
                out.extend([TokenTree::Group(ng)]);
            }
            TokenTree::Punct(p) if p.as_char() == '$' => {
                if let Some(TokenTree::Ident(i)) = iter.peek()
                    && i == "crate"
                {
                    let span = iter.next().map(|t| t.span()).unwrap_or_else(|| p.span());
                    out.extend([TokenTree::Ident(Ident::new("__sv_fragment_crate", span))]);
                } else {
                    out.extend([TokenTree::Punct(p)]);
                }
            }
            other => out.extend([other]),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `ready` dep marker's feature rejection. Not UI-testable: the
    /// dev-dependency supervisor carries `readiness` for the trybuild pass
    /// cases, and the scratch project's feature unification re-enables this
    /// crate's feature through the supervisor's weak forward — so the
    /// no-feature path can only be exercised here, on the parser directly.
    #[test]
    fn ready_marker_requires_feature() {
        let res = syn::parse_str::<GraphSpec>(
            "node NET = Terminate, deps: [];\n\
             node HTTP = Terminate, deps: [NET ready];",
        );
        if cfg!(feature = "readiness") {
            assert!(res.is_ok(), "marker accepted with the feature");
        } else {
            match res {
                Ok(_) => panic!("marker accepted without the feature"),
                Err(err) => assert!(
                    err.to_string().contains("requires the `readiness` feature"),
                    "unexpected error: {err}"
                ),
            }
        }
    }

    /// A stray ident after a dep name is rejected as an unknown marker in both
    /// feature states.
    #[test]
    fn unknown_dep_marker_rejected() {
        match syn::parse_str::<GraphSpec>("node A = Terminate, deps: [B rdy];") {
            Ok(_) => panic!("unknown marker accepted"),
            Err(err) => assert!(err.to_string().contains("`ready` marker"), "got: {err}"),
        }
    }
}
