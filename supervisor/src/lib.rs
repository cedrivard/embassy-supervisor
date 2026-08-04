// `no_std` for the shipped crate and the embedded build; under `cargo test` the
// crate is built for the host, where the test harness and the unit tests need `std`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
//! # embassy-supervisor — a task-lifecycle supervisor for [embassy](https://embassy.dev)
//!
//! Application- and HAL-agnostic primitives for orchestrating a set of embassy
//! tasks: bringing them up in dependency order, tearing them down in reverse,
//! scaling an elastic worker pool with load, placing nodes on interrupt-priority
//! tiers or a second core, and starting/stopping/pausing/resuming individual
//! tasks at runtime while keeping the dependency graph consistent. The supervisor
//! orchestrates task *lifecycle* and leaves the rest — allocation, HAL, power,
//! what the tasks do — to the application.
//!
//! ## The model
//!
//!   * The graph is declared once with the [`supervisor_graph!`] macro: each
//!     managed task becomes a [`TaskNode`] `static`, and the macro bundles the node
//!     slots, dependency table, and a topological order computed **at compile time**
//!     into a single [`Graph`] (`GRAPH`). The whole graph is validated at compile
//!     time — a dependency cycle, an unknown or duplicate dependency, a duplicate
//!     name, or bad pool bounds are compile errors.
//!   * [`Supervisor::new`] takes `&GRAPH` (no work, no failure) and uses the order
//!     to bring tasks up in dependency order ([`Supervisor::start`]) and tear them
//!     down in reverse ([`Supervisor::teardown`]).
//!   * `executor NAME;` items declare runtime-filled [`SpawnerSlot`]s, and
//!     `executor: NAME` on a node (or a whole pool) routes its spawn through one —
//!     an interrupt-priority tier or the second core. Bring-up *awaits* the slot
//!     (bounded), so an executor that comes up late — or on another core — is a
//!     rendezvous, not a race.
//!   * Each managed task names its worker with either `task:` (preferred) — a
//!     **plain `async fn`** that the macro wraps in a generated
//!     `#[embassy_executor::task]` shell (one concrete shell per declaration, so a
//!     *generic* worker is fine) — or `spawn:`, naming a hand-written
//!     `#[embassy_executor::task]` directly. A `task:` pool emits one shell sized to
//!     its members; `pool_size: N` sizes a single node's shell.
//!   * `resources: [NAME: Type, ..]` on a `task:` node threads **owned resources
//!     from `main`** into the worker through macro-emitted [`ResourceSlot`]s —
//!     compile-time exclusive ownership (the `Peripherals` field is consumed, no
//!     `steal()` inside the task), fail-closed provisioning (an unprovided slot
//!     fails `start` with `SpawnError::Busy`), and restore-on-exit so a respawn
//!     re-takes the *same instance*. Per-entry kind markers refine that
//!     default: `consume` hands the worker the value **by value** with no
//!     restore (drop-at-teardown drivers; rebuilt-per-cycle resources — a
//!     respawn fail-closes until the app re-`provide()`s); `shared` is a
//!     fan-out slot for a `Copy` handle (the glue copies via
//!     [`ResourceSlot::get`], the slot stays filled — any number of nodes and
//!     whole pools may declare the same name); and `local` swaps in a
//!     graph-site slot without the `T: Send` bound (`!Send` driver handles,
//!     single-core contract) — it makes the macro emit an `unsafe impl Sync`
//!     into the consuming crate, so it requires the non-default
//!     `local-resources` feature. See the macro docs for the markers' fine
//!     print.
//!   * The pre-spawn waits are per-node tunable (`slot_timeout:` /
//!     [`TaskNode::with_slot_timeout`]), which makes **provider nodes** work: a
//!     first-in-topo node whose worker *builds* resources at runtime and
//!     `provide()`s them into other nodes' slots (the graph-native `hw_init`);
//!     consumers size their timeout to the build and the gate wait becomes a
//!     rendezvous.
//!   * Two flags span every lifecycle operation: **disabled** (stopped until an
//!     explicit `Activate` — declared `disabled` in the graph or control-stopped;
//!     see [`TaskNode::set_disabled`]) and **detached** (self-managed: after
//!     [`TaskNode::set_detached`] no supervisor operation touches the node).
//!   * Each node carries a `TaskHandle` of per-node atomic flags and
//!     single-consumer `Signal`s. Every node is single-instance — no counts, no
//!     fan-out. See [`TaskHandle`].
//!
//! ## Three lifecycles, distinguished by [`Mode`]
//!
//!   * [`Mode::Terminate`] — the task exits its loop on shutdown and is respawned
//!     on the next bring-up. Stateless services (a network listener, a logger).
//!   * [`Mode::Pause`] — the task acks the shutdown then parks on
//!     `wait_resume()`; it is resumed in place, never respawned. Tasks that
//!     retain a resource across the pause (an open peripheral handle, a socket).
//!   * [`Mode::OnDemand`] — like `Terminate`, but not started at boot and not
//!     auto-respawned; the supervisor brings it up and down at runtime to scale
//!     an elastic worker pool ([`ElasticPool`]) with load.
//!
//! ## Writing a supervised task
//!
//! A supervised worker's first parameter is its node. With `task:` you write a
//! plain `async fn` and the macro stamps the `#[embassy_executor::task]` shell
//! (and, with `resources:`, hands it `&mut` resource handles after the node, in
//! declared order); with `spawn:` you write the `#[embassy_executor::task]`
//! yourself. Either way the macro's glue passes the node, and extra arguments come
//! from the partial-call spawn form. Four rules cover the task side of the protocol:
//!
//!   1. race long-lived work against the stop request — that's how a stop reaches
//!      you. [`TaskNode::run_cancellable_acked`] is the everyday body (it owns the
//!      `select` and acks for you; `Err(`[`Aborted`]`)` means a stop won),
//!      [`TaskNode::run_cancellable`] the variant with cleanup between the two, and
//!      [`TaskNode::wait_shutdown`] the raw signal when you write the `select`
//!      yourself;
//!   2. ack exactly once per stop with [`TaskNode::ack_dropped`]: on exit
//!      (`Terminate`/`OnDemand`), or on each pause (`Pause`) *before* parking on
//!      [`TaskNode::wait_resume`];
//!   3. an autonomous exit calls [`TaskNode::mark_exited`] instead — it acks *and*
//!      records completion, so the supervisor sees the node as down and
//!      [`TaskNode::has_exited`] tells a body that returned on its own from one
//!      that was stopped (a `task:` shell does it for you);
//!   4. resources follow the mode: a `Terminate` task re-acquires everything on
//!      respawn (drop-on-exit is the cleanup), a `Pause` task keeps what it holds
//!      across the park.
//!
//! Pool workers additionally report load with [`TaskNode::mark_busy`] /
//! [`TaskNode::mark_idle`] (a real transition fires the scale signal itself), and
//! a self-managed daemon or run-once job opts out of supervision with
//! [`TaskNode::set_detached`]. The README's *Writing supervised tasks* section has
//! per-mode skeletons.
//!
//! ## Beyond bring-up
//!
//!   * [`Supervisor::run`] is bring-up plus the driver loop (pool scaling and the
//!     control mailbox) in one call; it returns only on a [`RunError`], which the
//!     application escalates. Drive the pieces yourself when the loop must watch
//!     extra wake sources.
//!   * Every shutdown path is fallible, never a library panic: [`Supervisor::teardown`]
//!     aborts at the first node that misses its ack and returns a [`ShutdownTimeout`]
//!     naming it, [`Supervisor::teardown_continue`] presses on through the rest and
//!     reports the first failure at the end (the "hardware reset next anyway" path).
//!   * `exit: Type` on a node adds a typed exit-value slot the application awaits
//!     with [`ResourceSlot::wait_take`] — a run-once job hands its result back.
//!     `state: Type = expr` (feature `heap-state`) boxes per-activation state that
//!     is freed when the task exits, so a stopped subsystem costs no RAM.
//!   * Feature `readiness` separates *spawned* from *serving*: a task asserts
//!     `set_ready()` and a `deps: [NET ready]` edge makes bring-up (and pool growth)
//!     wait for it. Feature `liveness` adds a per-node heartbeat (`beat()` /
//!     `is_stale()`) for alive-but-wedged detection.
//!   * A graph can span crates: `supervisor_fragment!` declares a module's nodes and
//!     [`compose_graph!`] assembles the fragments into one graph. `name: IDENT;`
//!     gives a second graph in the same binary its own statics and [`Supervisor`],
//!     for a subordinate sub-graph an application starts and tears down as a unit.
//!
//! ## What the supervisor does *not* do
//!
//!   * It does not model any power-state transition (sleep/wake): it reacts to
//!     "teardown" and "bring-up" requests; the application drives them.
//!   * It does not allocate, and does no work at construction: the topological
//!     sort runs at compile time (see the `supervisor_graph!` macro).
//!   * It does not observe task internals. Tasks self-report their drop state via
//!     `ack_dropped()` / `mark_exited()`; a task that misses the ack window comes
//!     back as a [`ShutdownTimeout`] naming the node, for the application to act on.
//!   * It does not catch panics: a panicking task is not captured or restarted.
//!     Pair the supervisor with a hardware watchdog for crashes, and the `liveness`
//!     heartbeat for tasks that are alive but wedged.
//!
//! ## Cargo features
//!
//!   * `control` *(default)* — the runtime control plane: [`ControlOp`],
//!     [`request_control`], [`Supervisor::apply_control`].
//!   * `pool` *(default)* — elastic worker pools: [`ElasticPool`],
//!     [`Supervisor::run_pools`], and the `pools` field of [`Graph`].
//!   * `macros` *(default)* — the [`supervisor_graph!`] graph-declaration macro (and
//!     `supervisor_fragment!` / [`compose_graph!`]).
//!   * `local-resources` — permit the `local` resource kind; ⚠ opting in to the macro
//!     emitting a documented `unsafe impl Sync` into your crate.
//!   * `readiness` — `set_ready`/`clear_ready`/`wait_ready` plus the `ready` dep
//!     marker, gating bring-up and pool growth on *serving*, not merely spawned.
//!   * `liveness` — a per-node heartbeat: `beat()`, `ticks_since_beat()`,
//!     `is_stale(max_age)`. A fresh spawn counts as a beat.
//!   * `heap-state` — the `state: Type = expr` clause: per-activation boxed state,
//!     reclaimed on task exit; ⚠ emits a small `unsafe` fallible-boxing helper and
//!     needs a `#[global_allocator]`.
//!   * `defmt` — route the supervisor's logs through `defmt`; without it the log
//!     macros are no-ops.
//!   * `trace` family (all opt-in) — `trace`: the `trace` module's recorders consuming
//!     embassy-executor's `_embassy_trace_*` hooks; `trace-hooks`:
//!     `supervisor_graph!` also *defines* the hook symbols; `metadata-names`: node
//!     names stamped into task Metadata for external consumers (rtos-trace/
//!     SystemView) — independent of `trace`, so it needs no hook symbols and pairs
//!     with embassy's own `rtos-trace`; `trace-names`: shorthand for `trace` +
//!     `metadata-names`; `trace-nested`: preemption-exact accounting (a nested
//!     higher-tier poll credits its time back to the window it interrupted).
//!
//! Build with `default-features = false` for a minimal core that only does
//! dependency-ordered bring-up/teardown (drops the control plane and pools,
//! trimming flash and a couple of statics).
//!
//! ## Example
//!
//! [`supervisor_graph!`] declares the whole graph once — it generates the node
//! `static`s and a single [`Graph`] value `GRAPH` bundling the node slots, dep
//! table, and compile-time topological order (a dependency cycle is a compile
//! error), which [`Supervisor::new`] consumes.
//!
//! ```ignore
//! use embassy_executor::Spawner;
//! use embassy_supervisor::{supervisor_graph, RunError, Supervisor, TaskNode};
//!
//! // `app` depends on `net`; `task:` names a plain async worker fn the macro wraps
//! // in its `#[embassy_executor::task]` shell (`spawn:` takes one you wrote yourself).
//! supervisor_graph! {
//!     node NET = Terminate, deps: [], task: net_task;
//!     node APP = Terminate, deps: [NET], task: app_task;
//! }
//!
//! // Plain async fns taking the node first — no embassy attribute needed. The
//! // combinator owns the shutdown `select` and the ack.
//! async fn net_task(node: &'static TaskNode) {
//!     let _ = node.run_cancellable_acked(async { /* serve forever */ }).await;
//! }
//! async fn app_task(node: &'static TaskNode) {
//!     let _ = node.run_cancellable_acked(async { /* serve forever */ }).await;
//! }
//!
//! #[embassy_executor::task]
//! async fn supervisor_task(spawner: Spawner) {
//!     // Infallible: the order is precomputed, so a dependency cycle is a compile error.
//!     let sup = Supervisor::new(&GRAPH);
//!     // Brings up `net`, then `app`, then drives pool scaling and runtime control
//!     // requests (start/stop/pause/resume, applied in dependency order) forever;
//!     // returns only on error, which the application escalates — typically a panic
//!     // into a hardware-watchdog reset.
//!     match sup.run(spawner).await {
//!         RunError::Spawn(_) => panic!("bring-up failed"),
//!         RunError::Shutdown(e) => panic!("{} missed its shutdown ack", e.node.name),
//!     }
//!     // Call the pieces yourself (`start`, then a `select(run_pools, wait_control)`
//!     // loop) when the driver must watch extra wake sources.
//! }
//! ```
//!
//! The `firmware` crate in the [repository](https://github.com/cedrivard/embassy-supervisor)
//! is a complete working example (USB-net, an HTTP control plane, an elastic pool,
//! and OTA).

#[macro_use]
mod fmt;

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::Ordering;
use core::task::{Context, Poll};

use embassy_executor::{SendSpawner, SpawnError, Spawner};
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
#[cfg(feature = "control")]
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Timer, with_timeout};
use portable_atomic::AtomicBool;
#[cfg(any(feature = "trace", feature = "liveness"))]
use portable_atomic::AtomicU32;

// ─── Scale-request signal (task → supervisor) ──────────────────────────────
//
// Elastic pool workers fire this when their busy/idle status changes; the
// supervisor's `run_pools` loop awaits it and re-runs the pool policies
// (`ElasticPool`). Single-consumer `Signal`: many tasks may `signal()`, only the
// supervisor `wait()`s. This is the *only* path by which task status reaches the
// supervisor — it never polls.
#[cfg(feature = "pool")]
static SCALE_REQ: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Fire the scale-request signal. Called by a task on a busy/idle transition.
/// A no-op when the `pool` feature is disabled (no pools to re-evaluate).
pub fn request_scale() {
    #[cfg(feature = "pool")]
    SCALE_REQ.signal(());
}

/// Await the next scale request. The supervisor's driver loop selects this
/// against its other wake sources and runs the scaling policy on each wake.
#[cfg(feature = "pool")]
pub async fn wait_scale() {
    SCALE_REQ.wait().await;
}

// ─── Runtime control commands (app → supervisor) ───────────────────────────
//
// An application's control surface (e.g. a network endpoint) usually can't drive
// the supervisor directly: the `Supervisor` and the `Spawner` live on the
// supervisor task's stack, not in a `static`. So control is decoupled via this
// channel — the caller `request_control()`s a (node, op) pair; the supervisor's
// driver loop `wait_control()`s it and runs the dependency-honoring
// `apply_control`. A `Channel` (not a `Signal`) so back-to-back requests aren't
// coalesced; capacity 4 is ample for hand-driven control. Delivery is lossless:
// `request_control` awaits free capacity, and the sync `try_request_control`
// surfaces a full mailbox as an error instead of dropping the request — a
// silently vanished emergency stop is the one failure mode this mailbox is not
// allowed to have.

/// Which way to drive a node. Higher-level verbs fold onto these two:
/// `start`/`resume` → `Activate`, `stop`/`pause` → `Deactivate`. The concrete
/// mechanism (respawn vs resume vs leave-to-pool) is then chosen per node `Mode`
/// by the supervisor when it applies the command ([`Supervisor::apply_control`]).
#[cfg(feature = "control")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ControlOp {
    /// Bring the node up (start a stopped `Terminate` node, resume a `Pause` node).
    Activate,
    /// Take the node down (and its dependents, per the graph).
    Deactivate,
}

/// A runtime control request: drive `node` (and, per the dependency graph and
/// pool membership, the nodes it implies) in the `op` direction.
#[cfg(feature = "control")]
#[derive(Clone, Copy, Debug)]
pub struct ControlCommand {
    /// The node to drive.
    pub node: &'static TaskNode,
    /// The direction to drive it.
    pub op: ControlOp,
}

/// App → supervisor control mailbox. `&'static TaskNode` is `Copy + Sync`, so
/// the target rides the channel directly — no name lookup needed supervisor-side.
#[cfg(feature = "control")]
static CONTROL_REQ: Channel<CriticalSectionRawMutex, ControlCommand, 4> = Channel::new();

/// The control mailbox was full (4 outstanding requests) and the request was
/// not enqueued. Returned by [`try_request_control`]; retry after the
/// supervisor's driver loop has drained a command, or use the awaiting
/// [`request_control`] from async contexts.
#[cfg(feature = "control")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ControlQueueFull;

#[cfg(all(feature = "control", feature = "defmt"))]
impl defmt::Format for ControlQueueFull {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "control queue full");
    }
}

/// Enqueue a control request, waiting for mailbox capacity if it is full.
/// Lossless — the request is delivered once the supervisor's driver loop drains
/// an earlier command. Called by the application's control surface.
#[cfg(feature = "control")]
pub async fn request_control(node: &'static TaskNode, op: ControlOp) {
    CONTROL_REQ.send(ControlCommand { node, op }).await;
}

/// Non-blocking variant of [`request_control`] for sync contexts (ISRs,
/// callbacks). Fails with [`ControlQueueFull`] instead of dropping the request
/// when the mailbox is full — the caller decides whether to retry or surface it.
#[cfg(feature = "control")]
pub fn try_request_control(node: &'static TaskNode, op: ControlOp) -> Result<(), ControlQueueFull> {
    CONTROL_REQ
        .try_send(ControlCommand { node, op })
        .map_err(|_| ControlQueueFull)
}

/// Await the next control request. Selected by the supervisor's driver loop
/// against pool scaling and any other application wake sources.
#[cfg(feature = "control")]
pub async fn wait_control() -> ControlCommand {
    CONTROL_REQ.receive().await
}

/// Per-node timeout for `wait_dropped`. A task that doesn't ack within this
/// window is a bug (e.g. a missing `ack_dropped()` call) or a wedge; the
/// shutdown paths surface it as a [`ShutdownTimeout`] naming the node, and the
/// application decides the escalation. 2 s comfortably exceeds a typical task's
/// poll period and peripheral settle time.
const SHUTDOWN_ACK_TIMEOUT_MS: u64 = 2_000;

/// A node failed to ack a requested shutdown within `SHUTDOWN_ACK_TIMEOUT_MS`.
/// Returned by [`Supervisor::stop_node`], [`Supervisor::teardown`],
/// [`Supervisor::teardown_continue`] and (feature `control`)
/// [`Supervisor::apply_control`]. The node is still marked running; the sane
/// escalations are app-level — a hardware watchdog reset, `panic!`, or a retry.
#[derive(Clone, Copy, Debug)]
pub struct ShutdownTimeout {
    /// The node that missed its ack window.
    pub node: &'static TaskNode,
}

#[cfg(feature = "defmt")]
impl defmt::Format for ShutdownTimeout {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "{} missed shutdown ack", self.node.name);
    }
}

/// Why [`Supervisor::run`] stopped — it only returns on error, and every arm is
/// an app-level escalation (typically `panic!` into a hardware-watchdog reset).
#[cfg(any(feature = "pool", feature = "control"))]
#[derive(Clone, Copy, Debug)]
pub enum RunError {
    /// Bring-up failed: a spawn error out of the initial [`Supervisor::start`]
    /// (task-pool exhaustion, or a gate/slot wait that timed out as `Busy`).
    Spawn(SpawnError),
    /// A node missed its shutdown ack during a control cascade or pool shrink.
    Shutdown(ShutdownTimeout),
}

#[cfg(all(any(feature = "pool", feature = "control"), feature = "defmt"))]
impl defmt::Format for RunError {
    fn format(&self, fmt: defmt::Formatter) {
        match self {
            RunError::Spawn(_) => defmt::write!(fmt, "bring-up spawn failed"),
            RunError::Shutdown(e) => defmt::write!(fmt, "{}", e),
        }
    }
}

/// The shutdown side of [`TaskNode::run_cancellable`]'s result: the raced work
/// future was cancelled at its await point because a stop/pause request won the
/// select. Pairs naturally with the `exit:` slot — a worker returning
/// `Result<R, Aborted>` records completed-vs-cancelled for whoever reads the
/// exit value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Aborted;

#[cfg(feature = "defmt")]
impl defmt::Format for Aborted {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "aborted by shutdown");
    }
}

pin_project_lite::pin_project! {
    /// The future behind [`TaskNode::run_cancellable`] and
    /// [`run_cancellable_acked`](TaskNode::run_cancellable_acked): races the
    /// worker against the node's shutdown signal, holding the worker's state
    /// machine **once**.
    ///
    /// Written by hand rather than as `select(fut, wait_shutdown()).await` inside
    /// an `async fn`, because that shape stores the worker both as the function's
    /// argument and inside the select (rust-lang/rust#62958) — a doubling paid in
    /// every caller's static task storage, which for a graph of `cancel` nodes is
    /// the sum of every worker future in the binary.
    ///
    /// `fut` is an `Option` so the abort path can drop the worker in place, via
    /// the safe `Pin::set`, *before* acking: the ack is what releases the
    /// supervisor's teardown wait, and a runner whose `Drop` frees hardware must
    /// have run by then.
    struct RunCancellable<'a, F, S> {
        #[pin]
        fut: Option<F>,
        #[pin]
        shutdown: S,
        // `Some` only for the `_acked` variant.
        ack: Option<&'a TaskNode>,
    }
}

impl<F: Future, S: Future<Output = ()>> Future for RunCancellable<'_, F, S> {
    type Output = Result<F::Output, Aborted>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        // Worker first, shutdown second — the polling order `select` gave this
        // before, so a worker that completes in the same wake as a stop request
        // still reports completion rather than abort.
        if let Some(fut) = this.fut.as_mut().as_pin_mut()
            && let Poll::Ready(out) = fut.poll(cx)
        {
            return Poll::Ready(Ok(out));
        }
        match this.shutdown.poll(cx) {
            Poll::Ready(()) => {
                this.fut.set(None);
                if let Some(node) = this.ack {
                    node.ack_dropped();
                }
                Poll::Ready(Err(Aborted))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// How long the supervisor's bring-up waits for a node's `executor:`
/// [`SpawnerSlot`] to be filled before failing the spawn with
/// [`SpawnError::Busy`]. A genuine cross-core rendezvous resolves in microseconds;
/// a slot empty this long is a misconfiguration (the app never registered that
/// executor's spawner). Bounded, so a misconfigured graph fails loudly instead of
/// hanging bring-up forever.
const SLOT_READY_TIMEOUT: embassy_time::Duration = embassy_time::Duration::from_millis(100);

// ─── Mode ────────────────────────────────────────────────────────────────

/// Lifecycle policy for a managed task: what the task does on shutdown and what
/// the supervisor does to bring it back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Task exits its loop on shutdown. The supervisor respawns it via the
    /// node's `spawn` fn from `respawn_terminate`.
    Terminate,
    /// Task acks shutdown and parks on `wait_resume()`. The supervisor resumes
    /// it from `resume_pausable`; the task is never respawned, so it keeps any
    /// resource it holds (a peripheral handle, a socket) across the pause.
    Pause,
    /// Like `Terminate` (exits on shutdown), but **not** started at boot and
    /// **not** auto-respawned. The supervisor brings it up and down at runtime
    /// via `start_node` / `stop_node` in response to load — see [`ElasticPool`].
    /// `start()` skips it; `respawn_terminate()` leaves it down (it
    /// re-grows under demand); `teardown()` only acts on it while it is running.
    OnDemand,
}

impl Mode {
    /// Stable lower-case wire name, used both for serialization (e.g. a JSON
    /// task-state view) and for `defmt` logging — the single source of these
    /// strings.
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Terminate => "terminate",
            Mode::Pause => "pause",
            Mode::OnDemand => "ondemand",
        }
    }
}

#[cfg(feature = "defmt")]
impl defmt::Format for Mode {
    fn format(&self, f: defmt::Formatter) {
        defmt::write!(f, "{}", self.as_str());
    }
}

// ─── TaskHandle ──────────────────────────────────────────────────────────

/// Coordination state for one task. Embedded inside [`TaskNode`].
///
/// Every node is single-instance, so each field is a per-node atomic flag or a
/// single-consumer signal — no counts, no fan-out. Written by one side (task or
/// supervisor) and read by the other:
///   * `shutdown` / `shutdown_wake` — supervisor requests exit; the task parks
///     on the signal and reads the flag.
///   * `dropped` / `dropped_wake` — the task acks its exit; the supervisor
///     parks on the signal (with a timeout) and reads the flag.
///   * `resume_wake` — supervisor resumes a parked Pause-mode task.
///   * `running` — supervisor's record that the node is spawned; `busy` — the
///     task's active/idle status. Both read by the elastic scaling policy.
///   * `disabled` — the node has been manually deactivated; see below.
pub struct TaskHandle {
    /// Set true by the supervisor when shutdown is requested.
    /// Cleared by `reset()` before the next spawn.
    shutdown: AtomicBool,
    /// Wake source for `wait_shutdown()`. Fired by `signal_shutdown()`.
    shutdown_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Set true by the instance when it acks the shutdown (a bool, not a count,
    /// since every node is single-instance). Cleared by `reset()`.
    dropped: AtomicBool,
    /// Wake source for `wait_dropped()`. Fired by `ack_dropped()`.
    dropped_wake: Signal<CriticalSectionRawMutex, ()>,
    /// True while the supervisor has the node spawned and it hasn't exited.
    /// Always-on nodes are set true by `start()`; `OnDemand` nodes are set
    /// true/false by `start_node()` / `stop_node()`. `teardown()` only acts on
    /// `running` nodes, so a down `OnDemand` node doesn't stall it.
    running: AtomicBool,
    /// True while the task is actively serving (its active/idle status). Set by
    /// `mark_busy()` / `mark_idle()`; read by the scaling policy.
    busy: AtomicBool,
    /// Set true by `mark_exited()` when the task body has returned — by the
    /// generated `task:` shell automatically, or by a hand-written `spawn:` task
    /// on its way out. Cleared by `reset()` before the next spawn. Together with
    /// the lifecycle-spanning `shutdown` flag this distinguishes an autonomous
    /// completion (`completed && !shutdown`) from an acked stop.
    completed: AtomicBool,
    /// Wake source for `wait_resume()` on Pause-mode tasks. Fired by
    /// `signal_resume()`.
    resume_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Task-asserted readiness ("initialized and serving", e.g. DHCP bound) —
    /// distinct from `running` (spawned). Set by `set_ready()`, cleared by
    /// `clear_ready()` and by `reset()` so a respawned provider re-asserts.
    #[cfg(feature = "readiness")]
    ready: AtomicBool,
    /// Wake source for `wait_ready()`. Latching; the supervisor's bring-up is
    /// the only pre-fill waiter (single-waiter Signal semantics).
    #[cfg(feature = "readiness")]
    ready_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Instant ticks (truncated) of the last `beat()`; also stamped by
    /// `set_running(true)` so a freshly spawned node is never instantly stale.
    #[cfg(feature = "liveness")]
    last_beat: AtomicU32,
    /// True while the node has been manually deactivated (stopped/paused) via the
    /// runtime control interface (`Supervisor::deactivate`). Unlike the other
    /// flags this one is **lifecycle-spanning**: it is *not* cleared by
    /// `reset()`, so a manual stop "sticks" — the automatic bring-up paths
    /// (`start`, `respawn_terminate`, `resume_pausable`, and the elastic pool's
    /// grow) skip a node while it is set. Cleared only by `Supervisor::activate`.
    /// Because it lives in a `static`, it also survives a power-state transition
    /// that retains RAM (e.g. a warm-resume from deep sleep).
    disabled: AtomicBool,
    /// Self-managed: while set, the supervisor never drives this node — teardown,
    /// deactivate/activate, `stop_node`, respawn, and pause-resume all skip it. Not
    /// cleared by `reset()`. Full rationale on [`TaskNode::set_detached`].
    detached: AtomicBool,
    /// The executor task id currently running this node (`TaskRef::id()`, captured
    /// from the `SpawnToken` by the macro's spawn glue). `0` = unknown (not yet
    /// spawned, or a parked/closure-spawned node that never registered). Overwritten
    /// on every (re)spawn, so — unlike an external tracker — it stays correct across
    /// respawns without any unlinking.
    #[cfg(feature = "trace")]
    task_id: AtomicU32,
    /// Accumulated executor-poll time for this node, in embassy-time ticks,
    /// wrapping. Consumers sample twice and `wrapping_sub` to get a rate; the
    /// crate does no windowing.
    #[cfg(feature = "trace")]
    exec_ticks: AtomicU32,
    /// Number of executor polls of this node, wrapping.
    #[cfg(feature = "trace")]
    polls: AtomicU32,
    /// Longest single poll ever observed, in ticks — the "never yields" watermark.
    /// A large value names the node that hogged the executor even after the fact,
    /// which a live check cannot do from the blocked executor itself.
    #[cfg(feature = "trace")]
    max_poll_ticks: AtomicU32,
}

impl TaskHandle {
    const fn new(disabled_at_boot: bool) -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            shutdown_wake: Signal::new(),
            dropped: AtomicBool::new(false),
            dropped_wake: Signal::new(),
            running: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            resume_wake: Signal::new(),
            #[cfg(feature = "readiness")]
            ready: AtomicBool::new(false),
            #[cfg(feature = "readiness")]
            ready_wake: Signal::new(),
            #[cfg(feature = "liveness")]
            last_beat: AtomicU32::new(0),
            disabled: AtomicBool::new(disabled_at_boot),
            detached: AtomicBool::new(false),
            #[cfg(feature = "trace")]
            task_id: AtomicU32::new(0),
            #[cfg(feature = "trace")]
            exec_ticks: AtomicU32::new(0),
            #[cfg(feature = "trace")]
            polls: AtomicU32::new(0),
            #[cfg(feature = "trace")]
            max_poll_ticks: AtomicU32::new(0),
        }
    }
}

// ─── Executor spawner slots ──────────────────────────────────────────────

/// A runtime-filled slot holding the [`SendSpawner`] of an executor other than
/// the one the supervisor runs on — an `InterruptExecutor` tier, the second
/// core's executor, any foreign thread executor (via `Spawner::make_send()`).
///
/// Declared by the `executor NAME;` item of [`supervisor_graph!`]; nodes carrying
/// `executor: NAME` are spawned through the slot instead of the supervisor's own
/// `Spawner`. The application fills it once at startup — before, or concurrently
/// with, [`Supervisor::start`] (e.g. from the second core's bring-up):
///
/// ```ignore
/// static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();
/// HIGH.set(EXECUTOR_HIGH.start(interrupt::SWI_IRQ_0));
/// sup.start(spawner).await?;   // nodes declared `executor: HIGH` spawn on that tier
/// ```
///
/// The supervisor's bring-up (`start` / `start_node` / `respawn_terminate`) awaits
/// [`ready`](Self::ready) for a node's slot before spawning it, so a tier filled
/// late — or from another core — is handled without a race; a slot still empty after
/// the supervisor's bounded wait fails the spawn with [`SpawnError::Busy`] rather
/// than silently dropping the task. Spawned futures must be `Send` (a non-`Send`
/// `executor:` task is a compile error at the glue).
pub struct SpawnerSlot {
    slot: BlockingMutex<CriticalSectionRawMutex, Cell<Option<SendSpawner>>>,
    /// Wakes a `ready()` waiter when `set` fills the slot (cross-core safe:
    /// `Signal` is critical-section based and latches).
    filled: Signal<CriticalSectionRawMutex, ()>,
}

impl SpawnerSlot {
    /// An empty slot (`const` — it lives in a `static` the macro emits).
    pub const fn new() -> Self {
        Self {
            slot: BlockingMutex::new(Cell::new(None)),
            filled: Signal::new(),
        }
    }

    /// Fill the slot (last set wins) and wake a [`ready`](Self::ready) waiter.
    /// Call before [`Supervisor::start`] — or from the other core's bring-up,
    /// with the supervisor awaiting `ready()`.
    pub fn set(&self, spawner: SendSpawner) {
        self.slot.lock(|c| c.set(Some(spawner)));
        self.filled.signal(());
    }

    /// The registered spawner, or `None` while unfilled.
    pub fn get(&self) -> Option<SendSpawner> {
        self.slot.lock(Cell::get)
    }

    /// Await the slot and return the spawner. The rendezvous primitive: the
    /// supervisor's bring-up awaits this for a node's `executor:` slot before
    /// spawning it (bounded, see [`Supervisor::start`]), so a tier filled late — or
    /// from another core — is handled without a race. Returns immediately once the
    /// slot is filled, so any number of *late* callers are fine (an application can
    /// gate work on the executor being up). While the slot is still empty, at most
    /// one task should be parked here: the underlying `Signal` holds a single waker,
    /// so a second pre-fill waiter would displace the first.
    pub async fn ready(&self) -> SendSpawner {
        loop {
            if let Some(sp) = self.get() {
                return sp;
            }
            // `Signal` latches: a `set()` racing between the check above and
            // this wait still wakes us.
            self.filled.wait().await;
        }
    }
}

impl Default for SpawnerSlot {
    fn default() -> Self {
        Self::new()
    }
}

// ─── ResourceSlot ────────────────────────────────────────────────────────

/// Type-erased readiness view of a [`ResourceSlot`], for the supervisor's
/// bring-up wait.
///
/// A `TaskNode` can gate on any number of slots of *different* `T`s, so the node
/// stores `&'static [&'static dyn ResourceGate]` (object-safe: no `T` in the
/// signatures). Same shape as embassy's `dyn` driver registries — see
/// <https://doc.rust-lang.org/reference/items/traits.html#object-safety>.
/// The supervisor only needs "is it filled?" plus the signal to park on; taking
/// the value stays in the generated spawn glue, where the concrete `T` is known.
pub trait ResourceGate: Sync {
    /// Non-consuming "is the slot currently filled" check.
    fn is_filled(&self) -> bool;
    /// The latching [`Signal`] fired by `provide`/`restore`, for the supervisor's
    /// bounded pre-spawn wait (see [`Supervisor::start`]).
    fn filled_signal(&self) -> &Signal<CriticalSectionRawMutex, ()>;
}

/// A one-value handoff cell threading an owned resource from `main` into a
/// supervised task — the safe replacement for `Peripherals::steal()` inside
/// the task body.
///
/// Declared (as a `pub static`) by [`supervisor_graph!`] for each entry in a
/// node's `resources:` clause. The protocol:
///
/// 1. `main` splits `Peripherals` and **moves** the resource in with
///    [`provide`](Self::provide). This is where the compile-time guarantee
///    lives: the singleton field is *consumed*, so no second owner — and no
///    `unsafe` steal — can exist.
/// 2. The generated spawn glue [`take`](Self::take)s it just before spawning
///    the node. An empty slot fails the spawn with `SpawnError::Busy` — a
///    fail-closed error out of [`Supervisor::start`], not a panic inside the
///    task (compare `static_cell::StaticCell`, which panics on misuse).
/// 3. The generated task shell hands the worker `&mut T` and
///    [`restore`](Self::restore)s the value after the worker returns, so a
///    `Terminate` respawn re-takes the *same instance* instead of stealing a
///    fresh one. (A `Pause` worker never returns — it parks — so it simply
///    retains the resource, exactly like a hand-written parked task.)
///
/// Same primitives as [`SpawnerSlot`]: a critical-section
/// [`BlockingMutex`]`<`[`Cell`]`<Option<T>>>` for the value (`Sync` for
/// `T: Send`, provided by embassy-sync — no `unsafe` here) plus a latching
/// [`Signal`] so the supervisor can await late provisioning (bounded; see
/// [`Supervisor::start`]).
pub struct ResourceSlot<T> {
    slot: BlockingMutex<CriticalSectionRawMutex, Cell<Option<T>>>,
    /// Wakes the supervisor's pre-spawn wait when `provide`/`restore` fills the
    /// slot (latching, so a fill racing the check-then-wait still wakes it).
    filled: Signal<CriticalSectionRawMutex, ()>,
}

impl<T> ResourceSlot<T> {
    /// An empty slot (`const` — it lives in a `static` the macro emits).
    pub const fn new() -> Self {
        Self {
            slot: BlockingMutex::new(Cell::new(None)),
            filled: Signal::new(),
        }
    }

    /// Move the resource in (from `main`'s `Peripherals` split) and wake the
    /// supervisor's pre-spawn wait. Call before [`Supervisor::start`]; a slot
    /// still empty after the supervisor's bounded wait fails that node's spawn
    /// with `SpawnError::Busy`. Filling an occupied slot replaces (drops) the
    /// old value — don't: one resource, one slot, moved exactly once.
    pub fn provide(&self, value: T) {
        self.slot.lock(|c| c.set(Some(value)));
        self.filled.signal(());
    }

    /// Take the resource out, leaving the slot empty. Called by the generated
    /// spawn glue just before the spawn; `None` means "not provided yet" or
    /// "currently held by a live task instance".
    pub fn take(&self) -> Option<T> {
        self.slot.lock(Cell::take)
    }

    /// Copy the resource out **without emptying the slot** — the `shared`
    /// resource kind's read: any number of consumers (several nodes, a whole
    /// pool) get the same `Copy` handle, and the slot stays filled for the
    /// next one. Only for `T: Copy` (a `Stack`-like handle, a `&'static`
    /// registry ref); an owned singleton uses [`take`](Self::take).
    pub fn get(&self) -> Option<T>
    where
        T: Copy,
    {
        // Same peek shape as `is_filled`: `Cell` has no `&T` access, so
        // take-copy-put-back under one critical section.
        self.slot.lock(|c| {
            let v = c.take();
            c.set(v);
            v
        })
    }

    /// Put the resource back for the next spawn. Called by the generated task
    /// shell after the worker returns (i.e. after its clean shutdown ack), so a
    /// respawn re-takes the same instance.
    pub fn restore(&self, value: T) {
        self.provide(value);
    }

    /// Await the slot being filled, then take the value — how an application
    /// reads a node's `exit:` slot (the shell `provide()`s the worker's return
    /// value there just before recording the exit). Check-then-park, so a value
    /// provided earlier is returned immediately; the latching signal carries
    /// the same single-pre-fill-waiter caveat as [`SpawnerSlot::ready`] — for
    /// N concurrent readers fan out through an app-owned `Watch` instead.
    pub async fn wait_take(&self) -> T {
        loop {
            if let Some(v) = self.take() {
                return v;
            }
            self.filled.wait().await;
        }
    }
}

// `T: Send` (not just any `T`): the gate is reachable from the supervisor task,
// which may run on a different core than the provider — the same bound the
// inner `BlockingMutex` requires for `Sync`, restated here so the `dyn` upcast
// can't outrun it.
impl<T: Send> ResourceGate for ResourceSlot<T> {
    fn is_filled(&self) -> bool {
        // Peek without consuming: `Cell` has no `&T` access (no `T: Copy`
        // here), so take-and-put-back under the same critical section.
        self.slot.lock(|c| {
            let v = c.take();
            let filled = v.is_some();
            c.set(v);
            filled
        })
    }

    fn filled_signal(&self) -> &Signal<CriticalSectionRawMutex, ()> {
        &self.filled
    }
}

impl<T> Default for ResourceSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ─── TaskNode ────────────────────────────────────────────────────────────

/// A node in the supervisor's task graph.
///
/// Designed to live in `static` memory: every field is `Sync`, all constructors
/// are `const`. Declared by [`supervisor_graph!`], which emits one per managed
/// task along with the [`Graph`] (`GRAPH`) that [`Supervisor::new`] consumes.
pub struct TaskNode {
    /// Human-readable name. Used in defmt logs and panic messages.
    pub name: &'static str,
    /// Lifecycle policy. See [`Mode`].
    pub mode: Mode,
    /// App-provided spawn function (typically an inline closure at the node's
    /// declaration). Called once at boot from `Supervisor::start`, again from
    /// `respawn_terminate` for Terminate nodes, and at runtime from `start_node`
    /// for `OnDemand` nodes. `None` for a **parked** node the application spawns
    /// itself (e.g. a `Pause` sensor holding a peripheral handle): the supervisor
    /// tracks its lifecycle but never spawns it.
    pub spawn: Option<fn(Spawner) -> Result<(), SpawnError>>,
    /// The executor [`SpawnerSlot`] this node spawns through (`executor: NAME` in
    /// the graph), or `None` to spawn on the supervisor's own `Spawner`. When
    /// `Some`, the supervisor awaits the slot's [`ready`](SpawnerSlot::ready)
    /// (bounded by [`SLOT_READY_TIMEOUT`]) *before* invoking `spawn`, so the
    /// generated glue's own non-blocking `SpawnerSlot::get` is already filled. Set
    /// by the macro via [`with_executor`](Self::with_executor); `const`, zero-cost.
    spawn_slot: Option<&'static SpawnerSlot>,
    /// The [`ResourceSlot`]s this node's spawn takes from (`resources:` in the
    /// graph), type-erased to their [`ResourceGate`] readiness view. The
    /// supervisor awaits every gate being filled (bounded by
    /// [`SLOT_READY_TIMEOUT`]) *before* invoking `spawn`, so (a) a `main` that
    /// provides late is tolerated and (b) a respawn cannot race the previous
    /// instance's shell restoring the value (the restore happens after the
    /// worker's shutdown ack). Empty for nodes without `resources:`. Set by the
    /// macro via [`with_resources`](Self::with_resources); `const`, zero-cost.
    resource_gates: &'static [&'static dyn ResourceGate],
    /// Deps whose task-asserted readiness (`set_ready`) bring-up awaits before
    /// spawning this node — the `ready`-marked subset of `deps:`. Spawn-order
    /// deps stay in the graph's dep table; this is the readiness overlay.
    #[cfg(feature = "readiness")]
    ready_deps: &'static [&'static TaskNode],
    /// Bound on the pre-spawn waits for this node's `executor:` slot and
    /// `resources:` gates. Defaults to [`SLOT_READY_TIMEOUT`] (100 ms — sized
    /// for "main provided before start"); raise it (`slot_timeout:` in the
    /// graph) for a node whose slots are filled by a **provider node** at
    /// runtime — e.g. an async radio bring-up worth hundreds of milliseconds.
    /// Set by the macro via [`with_slot_timeout`](Self::with_slot_timeout).
    slot_timeout: embassy_time::Duration,
    handle: TaskHandle,
}

impl TaskNode {
    /// A single-instance node started at boot (`Terminate`/`Pause`) or on demand
    /// (`Mode::OnDemand`). Every node is single-instance; an elastic service is
    /// modelled as several `OnDemand` nodes of the same pooled task fn.
    ///
    /// A `TaskNode` carries only its own identity and behaviour; the graph's
    /// dependency edges live in the compile-time index table that
    /// [`supervisor_graph!`] emits and [`Supervisor::new`] consumes.
    /// `disabled_at_boot` seeds the node's disabled flag so a control-started node
    /// (e.g. an OTA task) can be declared down and started later via a control op.
    /// `spawn` is `None` for a parked node the application spawns itself.
    pub const fn new(
        name: &'static str,
        mode: Mode,
        spawn: Option<fn(Spawner) -> Result<(), SpawnError>>,
        disabled_at_boot: bool,
    ) -> Self {
        Self {
            name,
            mode,
            spawn,
            spawn_slot: None,
            resource_gates: &[],
            #[cfg(feature = "readiness")]
            ready_deps: &[],
            slot_timeout: SLOT_READY_TIMEOUT,
            handle: TaskHandle::new(disabled_at_boot),
        }
    }

    /// Route this node's spawn through the given executor [`SpawnerSlot`] (the
    /// `executor: NAME` graph annotation). The supervisor awaits the slot before
    /// spawning the node, so a tier filled late — or from another core — is handled
    /// without a race, and the generated glue's non-blocking `get` is already filled.
    /// `const` and chainable in a `static` initializer; emitted by [`supervisor_graph!`].
    pub const fn with_executor(mut self, slot: &'static SpawnerSlot) -> Self {
        self.spawn_slot = Some(slot);
        self
    }

    /// Declare the [`ResourceSlot`]s this node's spawn takes from (the
    /// `resources:` graph clause). The supervisor awaits every gate being
    /// filled before spawning the node, so the generated glue's non-blocking
    /// `take()` finds the value. `const` and chainable in a `static`
    /// initializer; emitted by [`supervisor_graph!`].
    pub const fn with_resources(mut self, gates: &'static [&'static dyn ResourceGate]) -> Self {
        self.resource_gates = gates;
        self
    }

    /// Declare the deps whose task-asserted readiness bring-up awaits before
    /// spawning this node (the `ready`-marked subset of `deps:`). `const` and
    /// chainable in a `static` initializer; emitted by [`supervisor_graph!`].
    #[cfg(feature = "readiness")]
    pub const fn with_ready_deps(mut self, deps: &'static [&'static TaskNode]) -> Self {
        self.ready_deps = deps;
        self
    }

    /// Override the pre-spawn slot/gate wait bound for this node (the
    /// `slot_timeout: <millis>` graph clause). The default
    /// (`SLOT_READY_TIMEOUT`, 100 ms) assumes slots are provided *before*
    /// `start()`; a node consuming a **provider node's** outputs must cover the
    /// provider's async build time (the failure mode stays a loud
    /// `SpawnError::Busy`, just later). `const` and chainable in a `static`
    /// initializer; emitted by [`supervisor_graph!`].
    pub const fn with_slot_timeout(mut self, timeout: embassy_time::Duration) -> Self {
        self.slot_timeout = timeout;
        self
    }

    // ── Task-side API ────────────────────────────────────────────────────
    //
    // Called from inside the `#[embassy_executor::task] async fn` body. The
    // whole task-side protocol is four rules (the README's "Writing supervised
    // tasks" section has per-mode skeletons):
    //   1. select long-lived work against `wait_shutdown()`;
    //   2. `ack_dropped()` exactly once per stop — on exit (Terminate/OnDemand)
    //      or on each pause (Pause), before parking on `wait_resume()`;
    //   3. an autonomous exit calls `mark_exited()` (acks + records completion;
    //      `task:` shells do it automatically);
    //   4. resources follow the mode: Terminate re-acquires on respawn, Pause
    //      retains across park.

    /// True iff the supervisor has requested shutdown. Checked at the loop top
    /// alongside `wait_shutdown()` in a `select`.
    pub fn shutdown_requested(&self) -> bool {
        self.handle.shutdown.load(Ordering::Acquire)
    }

    /// Park until shutdown is requested. Returns immediately if shutdown has
    /// already been requested. Use this for single-instance tasks in a `select`
    /// against the task's main work future.
    pub async fn wait_shutdown(&self) {
        // Fast path — already requested. (Important because the signal is
        // edge-triggered: if `signal()` fired before we got here, the bare
        // `wait()` below would block forever.)
        if self.handle.shutdown.load(Ordering::Acquire) {
            return;
        }
        self.handle.shutdown_wake.wait().await;
    }

    /// Mark this instance as having shut down: clears the running flag and acks
    /// the teardown handshake (so the supervisor's `wait_dropped` completes).
    /// Every instance must call this exactly once on exit (Terminate/OnDemand
    /// mode) or on each pause (Pause mode). It also covers an **autonomous** exit
    /// the supervisor didn't request — e.g. a pool worker backing off — so the
    /// pool sees the instance as down and can re-grow it under later demand.
    pub fn ack_dropped(&self) {
        self.handle.running.store(false, Ordering::Release);
        self.handle.dropped.store(true, Ordering::Release);
        self.handle.dropped_wake.signal(());
    }

    /// Record that this node's task body has **returned**. Called automatically
    /// by the generated `task:` shell after the worker returns (and after
    /// resource restores); call it manually at the end of a hand-written
    /// `spawn:` task that can exit, where you would previously have called
    /// [`ack_dropped`](Self::ack_dropped) alone. Idempotent, and subsumes
    /// `ack_dropped`: it acks the teardown handshake *and* records completion,
    /// so a body that returns on its own — the case the supervisor previously
    /// could not observe — reads as down ([`is_running`](Self::is_running) →
    /// `false`, [`has_exited`](Self::has_exited) → `true`) instead of running
    /// forever, and a control `Activate` can respawn it.
    pub fn mark_exited(&self) {
        self.handle.completed.store(true, Ordering::Release);
        self.ack_dropped();
    }

    /// True once the last instance's body returned — set by
    /// [`mark_exited`](Self::mark_exited), cleared by the pre-spawn reset.
    /// `has_exited() && !shutdown_requested()` distinguishes an autonomous
    /// completion from an acked stop (the shutdown flag persists until the next
    /// reset).
    pub fn has_exited(&self) -> bool {
        self.handle.completed.load(Ordering::Acquire)
    }

    /// Assert readiness: "initialized and serving" (DHCP bound, registration
    /// done, calibration finished) — the task-side half of a `ready`-marked
    /// dependency edge. Distinct from *running* (spawned): `deps:` orders
    /// spawns; a `deps: [THIS ready]` edge additionally awaits this call.
    /// Latching until [`clear_ready`](Self::clear_ready) or the pre-spawn
    /// reset (a respawned provider re-asserts).
    #[cfg(feature = "readiness")]
    pub fn set_ready(&self) {
        self.handle.ready.store(true, Ordering::Release);
        self.handle.ready_wake.signal(());
    }

    /// Withdraw readiness — **status, not control**: dependents are NOT stopped
    /// or notified (pair with a control `Deactivate` for a cascade); it defers
    /// future bring-up (a ready-marked dependent's spawn, pool growth) until
    /// [`set_ready`](Self::set_ready) again. Use for "link lost, still
    /// reconnecting" style states.
    #[cfg(feature = "readiness")]
    pub fn clear_ready(&self) {
        self.handle.ready.store(false, Ordering::Release);
    }

    /// True while the node asserts readiness. Pool growth checks this for
    /// `ready`-marked deps; also useful in app health views.
    #[cfg(feature = "readiness")]
    pub fn is_ready(&self) -> bool {
        self.handle.ready.load(Ordering::Acquire)
    }

    /// Park until this node asserts readiness (immediately if it already has).
    /// The supervisor's bring-up is the intended pre-fill waiter; the latching
    /// signal has the same single-pre-fill-waiter caveat as
    /// [`SpawnerSlot::ready`] — for N concurrent app-side waiters fan out
    /// through an app-owned `embassy_sync::watch::Watch` fed by the ready task.
    #[cfg(feature = "readiness")]
    pub async fn wait_ready(&self) {
        loop {
            if self.is_ready() {
                return;
            }
            self.handle.ready_wake.wait().await;
        }
    }

    /// True when every `ready`-marked dep currently asserts readiness — the
    /// sync form pool growth uses (no wait: a not-ready dep just defers the
    /// grow to the next evaluation).
    #[cfg(all(feature = "pool", feature = "readiness"))]
    pub(crate) fn ready_deps_ok(&self) -> bool {
        self.ready_deps.iter().all(|d| d.is_ready())
    }
    #[cfg(all(feature = "pool", not(feature = "readiness")))]
    pub(crate) fn ready_deps_ok(&self) -> bool {
        true
    }

    /// Record a liveness heartbeat. Call once per work loop (or per served
    /// request); an app watchdog task reads [`is_stale`](Self::is_stale).
    #[cfg(feature = "liveness")]
    pub fn beat(&self) {
        self.handle.last_beat.store(
            embassy_time::Instant::now().as_ticks() as u32,
            Ordering::Release,
        );
    }

    /// Ticks since the last [`beat`](Self::beat) (wrapping arithmetic; correct
    /// for gaps under the u32 tick wrap, ~71 min at 1 MHz — far above any sane
    /// `max_age`).
    #[cfg(feature = "liveness")]
    pub fn ticks_since_beat(&self) -> u32 {
        (embassy_time::Instant::now().as_ticks() as u32)
            .wrapping_sub(self.handle.last_beat.load(Ordering::Acquire))
    }

    /// True when the node is running but hasn't beaten within `max_age` — the
    /// alive-but-wedged detector (a task hogging nothing, parked on an await
    /// that will never complete). Not-running nodes are never stale: a stopped
    /// or completed node is *down*, which `is_running`/`has_exited` already
    /// report. Complements the `trace` stall watermark, which catches the
    /// opposite failure (a poll that never yields).
    #[cfg(feature = "liveness")]
    pub fn is_stale(&self, max_age: embassy_time::Duration) -> bool {
        self.is_running() && u64::from(self.ticks_since_beat()) > max_age.as_ticks()
    }

    /// Pause-mode only: park until the supervisor signals resume. Call *after*
    /// [`ack_dropped`](Self::ack_dropped) — ack the pause, then park; held
    /// resources stay owned across the park.
    pub async fn wait_resume(&self) {
        self.handle.resume_wake.wait().await;
    }

    /// Race `fut` against this node's shutdown: `Ok(output)` when the work
    /// completes, `Err(Aborted)` when a stop/pause request wins. Owns the
    /// `select` that rule 1 of the task protocol otherwise has you write by
    /// hand. Does **not** ack — run your cleanup, then call
    /// [`ack_dropped`](Self::ack_dropped) (or return through
    /// [`run_cancellable_acked`](Self::run_cancellable_acked) when there is no
    /// cleanup between the select and the ack).
    ///
    /// ```ignore
    /// match node.run_cancellable(conn.serve()).await {
    ///     Ok(done) => handle(done),
    ///     Err(Aborted) => { flush().await; node.ack_dropped(); return; }
    /// }
    /// ```
    ///
    /// Returns a future rather than being an `async fn` on purpose: an `async fn`
    /// keeps `fut` in its own frame *and* in the `select` that lives across the
    /// await, and rustc does not overlap the two slots
    /// (rust-lang/rust#62958) — so the worker's state machine would be reserved
    /// twice in the caller's static storage. The hand-written future below holds
    /// it exactly once.
    pub fn run_cancellable<F: Future>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, Aborted>> {
        RunCancellable {
            fut: Some(fut),
            shutdown: self.wait_shutdown(),
            ack: None,
        }
    }

    /// [`run_cancellable`](Self::run_cancellable) that additionally calls
    /// [`ack_dropped`](Self::ack_dropped) before returning `Err(Aborted)` — for
    /// bodies with no teardown work between the select and the ack, e.g. a
    /// runner whose drop *is* the cleanup:
    ///
    /// ```ignore
    /// let _ = node.run_cancellable_acked(runner.run()).await; // drop releases the pins
    /// ```
    /// Single-copy for the same reason as
    /// [`run_cancellable`](Self::run_cancellable), and it keeps that method's
    /// ordering: on abort the worker future is dropped **before** the ack, so a
    /// runner whose `Drop` is the cleanup has released everything by the time the
    /// supervisor observes the handshake.
    pub fn run_cancellable_acked<F: Future>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, Aborted>> {
        RunCancellable {
            fut: Some(fut),
            shutdown: self.wait_shutdown(),
            ack: Some(self),
        }
    }

    /// Report that this task started serving a request (active). Fires the
    /// scale-request signal on a real idle→busy transition so the scaling policy
    /// can react (e.g. grow the pool); a redundant call doesn't re-signal.
    pub fn mark_busy(&self) {
        if !self.handle.busy.swap(true, Ordering::Release) {
            request_scale();
        }
    }

    /// Report that this task finished serving and is idle again. Fires the
    /// scale-request signal on a real busy→idle transition so the scaling policy
    /// can react (e.g. shrink the pool); a redundant call doesn't re-signal.
    pub fn mark_idle(&self) {
        if self.handle.busy.swap(false, Ordering::Release) {
            request_scale();
        }
    }

    /// True while this task is actively serving. Read by the scaling policy.
    pub fn is_busy(&self) -> bool {
        self.handle.busy.load(Ordering::Acquire)
    }

    /// True while the supervisor has this node spawned (and it hasn't exited).
    /// Read by the scaling policy to count live instances, and by a task-state
    /// view.
    pub fn is_running(&self) -> bool {
        self.handle.running.load(Ordering::Acquire)
    }

    /// True while the node is disabled: declared `disabled` in the graph
    /// (stopped-at-boot, up on an explicit `Activate`), or manually deactivated
    /// via the control interface and not yet re-activated. Read by a task-state
    /// view and by the automatic bring-up paths (which skip a disabled node).
    pub fn is_disabled(&self) -> bool {
        self.handle.disabled.load(Ordering::Acquire)
    }

    /// Mark/clear this node as **detached**: a self-managing node the supervisor
    /// brings up once (via [`start`](Supervisor::start)) and then stops managing
    /// **entirely**. Every runtime lifecycle operation skips a detached node: full
    /// [`teardown`](Supervisor::teardown), the control deactivate/activate cascades,
    /// [`stop_node`](Supervisor::stop_node), [`respawn_terminate`](Supervisor::respawn_terminate),
    /// and pause-resume. It keeps running (or, for a one-shot, stays exited) across a
    /// teardown/wake cycle instead of being stopped, re-enabled, or re-spawned. Use it
    /// for a task that must outlive the teardown it participates in — e.g. a sleep/power
    /// coordinator that tears the graph down, sleeps, then wakes it — or a self-managed
    /// one-shot whose `deps:` exist only for start-ordering. The node owns its own
    /// shutdown; the supervisor will not drive it.
    pub fn set_detached(&self, detached: bool) {
        self.handle.detached.store(detached, Ordering::Release);
    }

    /// True while this node is [detached](Self::set_detached): self-managed, skipped by
    /// every runtime lifecycle operation (teardown, deactivate/activate, `stop_node`,
    /// respawn, pause-resume). Only the initial `start` brings it up.
    pub fn is_detached(&self) -> bool {
        self.handle.detached.load(Ordering::Acquire)
    }

    // ── Trace/observability API (features `trace`/`trace-names`) ───────────

    /// Record the executor task id (`SpawnToken::id()` / `TaskRef::id()`) currently
    /// backing this node, so the [`trace`] recorders can attribute executor polls to
    /// it. Called automatically by the spawn glue `supervisor_graph!` generates;
    /// call it manually only for a **parked** node (no `spawn:`) or a verbatim-closure
    /// `spawn:`, where the macro cannot see the token. Overwrites on every (re)spawn.
    #[cfg(feature = "trace")]
    pub fn set_task_id(&self, id: u32) {
        self.handle.task_id.store(id, Ordering::Release);
    }

    /// Register an externally-spawned token as this node's live task: records
    /// the task id for the [`trace`] recorders and (feature `metadata-names`)
    /// stamps the node name into the task Metadata. One call replaces the
    /// manual [`set_task_id`](Self::set_task_id) dance wherever the macro can't
    /// see the token — parked nodes and verbatim-closure `spawn:` forms:
    ///
    /// ```ignore
    /// let t = environment_task(i2c_dev)?;
    /// BME280.adopt(&t);
    /// high_spawner.spawn(t);
    /// ```
    #[cfg(feature = "trace")]
    pub fn adopt<S>(&self, token: &embassy_executor::SpawnToken<S>) {
        self.set_task_id(token.id());
        #[cfg(feature = "metadata-names")]
        self.stamp_name(token);
    }

    /// Stamp this node's name into the task's embassy `Metadata` (feature
    /// `metadata-names`), so external consumers — rtos-trace/SystemView, debuggers —
    /// show the graph node name instead of an opaque task id. Unlike
    /// [`adopt`](Self::adopt) this does **not** capture the task id or touch the
    /// supervisor's [`trace`] recorders, so it needs neither the `trace` feature nor
    /// the `_embassy_trace_*` hook symbols: it is the name-only spawn path emitted
    /// when `metadata-names` is on but `trace` is off (pair it with embassy's
    /// `rtos-trace`). Called automatically by the spawn glue; call it manually only
    /// for a parked or verbatim-closure node the macro can't see.
    ///
    /// Requires `embassy-executor`'s `metadata-name` feature, which `metadata-names`
    /// pulls in; without a registered name the task keeps embassy's default.
    #[cfg(feature = "metadata-names")]
    pub fn stamp_name<S>(&self, token: &embassy_executor::SpawnToken<S>) {
        token.metadata().set_name(self.name);
    }

    /// The executor task id last recorded by [`set_task_id`](Self::set_task_id)
    /// (`0` = never spawned / not registered).
    #[cfg(feature = "trace")]
    pub fn task_id(&self) -> u32 {
        self.handle.task_id.load(Ordering::Acquire)
    }

    /// Accumulated executor-poll time of this node, in embassy-time ticks. Wrapping:
    /// sample twice and `wrapping_sub` the readings to get a rate over a window.
    #[cfg(feature = "trace")]
    pub fn exec_ticks(&self) -> u32 {
        self.handle.exec_ticks.load(Ordering::Relaxed)
    }

    /// Number of executor polls of this node (wrapping counter).
    #[cfg(feature = "trace")]
    pub fn poll_count(&self) -> u32 {
        self.handle.polls.load(Ordering::Relaxed)
    }

    /// Longest single executor poll of this node ever observed, in ticks — the
    /// "never yields" watermark. A poll is expected to be microseconds; a large
    /// value names the node that hogged its executor, even after the fact.
    #[cfg(feature = "trace")]
    pub fn max_poll_ticks(&self) -> u32 {
        self.handle.max_poll_ticks.load(Ordering::Relaxed)
    }

    // ── Supervisor-side API ──────────────────────────────────────────────
    //
    // Driven by the `Supervisor` struct. Kept `pub(crate)` so app code doesn't
    // accidentally bypass the supervisor's orchestration.

    pub(crate) fn signal_shutdown(&self) {
        self.handle.shutdown.store(true, Ordering::Release);
        self.handle.shutdown_wake.signal(());
    }

    pub(crate) fn signal_resume(&self) {
        self.handle.resume_wake.signal(());
    }

    pub(crate) fn set_running(&self, running: bool) {
        self.handle.running.store(running, Ordering::Release);
        // Stamp a beat at spawn so a freshly running node is never instantly
        // stale (its body may not reach its first beat() for a while).
        #[cfg(feature = "liveness")]
        if running {
            self.handle.last_beat.store(
                embassy_time::Instant::now().as_ticks() as u32,
                Ordering::Release,
            );
        }
    }

    /// Set/clear the manual-deactivation flag. Set by `Supervisor::deactivate`,
    /// cleared by `Supervisor::activate`. Deliberately *not* touched by
    /// `reset()`, so a manual stop survives respawn cycles and RAM-retaining
    /// power-state transitions.
    ///
    /// Public so an application can pre-disable a `Terminate` node *before*
    /// `Supervisor::start`, making it a stopped-at-boot task that only comes up on
    /// an explicit `Activate` control (a node started by control rather than at boot).
    pub fn set_disabled(&self, disabled: bool) {
        self.handle.disabled.store(disabled, Ordering::Release);
    }

    /// Wait until the instance has called `ack_dropped()`. Single-instance, so
    /// one ack ends the wait. The fast-path flag check handles the ack landing
    /// before this await (the `dropped_wake` signal is edge-triggered).
    /// True when an instance acked a stop WITHOUT exiting — for a `Pause` node
    /// that is exactly "parked on `wait_resume()`" (the protocol acks, then
    /// parks; a full exit would have set `completed` via `mark_exited`).
    /// Readable only before the pre-spawn `reset()` clears both flags.
    pub(crate) fn has_acked_stop(&self) -> bool {
        self.handle.dropped.load(Ordering::Acquire)
            && !self.handle.completed.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_dropped(&self) {
        if self.handle.dropped.load(Ordering::Acquire) {
            return;
        }
        self.handle.dropped_wake.wait().await;
    }

    /// Clear the shutdown flag, dropped flag, busy flag, completed flag, and the
    /// shutdown / dropped wake-signals so the next cycle starts clean. Doesn't
    /// touch `running` (managed around spawn/stop), `resume_wake`
    /// (`resume_pausable` fires that for Pause nodes), or `disabled`
    /// (lifecycle-spanning).
    pub(crate) fn reset(&self) {
        self.handle.shutdown.store(false, Ordering::Release);
        self.handle.dropped.store(false, Ordering::Release);
        self.handle.busy.store(false, Ordering::Release);
        self.handle.completed.store(false, Ordering::Release);
        // A respawned provider must re-assert readiness for its new instance.
        #[cfg(feature = "readiness")]
        {
            self.handle.ready.store(false, Ordering::Release);
            self.handle.ready_wake.reset();
        }
        self.handle.shutdown_wake.reset();
        self.handle.dropped_wake.reset();
    }
}

/// Manual impl: the private `TaskHandle` (Signals + atomics) has no `Debug`, and a
/// snapshot of the *live* flags is more useful than raw handle internals anyway.
/// `finish_non_exhaustive` marks the elided fields (`spawn`, the handle).
impl core::fmt::Debug for TaskNode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TaskNode")
            .field("name", &self.name)
            .field("mode", &self.mode)
            .field("running", &self.is_running())
            .field("busy", &self.is_busy())
            .field("disabled", &self.is_disabled())
            .field("detached", &self.is_detached())
            .finish_non_exhaustive()
    }
}

// ─── Graph ───────────────────────────────────────────────────────────────

/// The compile-time task graph produced by [`supervisor_graph!`]: the node slots,
/// the dependency-index table, the topological order, and the elastic pools — the
/// single value [`Supervisor::new`] consumes. The macro emits one `pub static GRAPH`
/// of this type. The fields are public so the application can read them directly
/// (e.g. a status endpoint iterating `GRAPH.nodes` / `GRAPH.deps`).
///
/// `N` is capped at 256 (graph indices are `u8`); the macro enforces this at
/// expansion time.
pub struct Graph<const N: usize> {
    /// Node slots, one per declared node. `None` marks a `#[cfg]`-ed-out node.
    pub nodes: &'static [Option<&'static TaskNode>; N],
    /// Per-node dependency indices into `nodes` (`deps[i]` lists node `i`'s deps).
    pub deps: &'static [&'static [u8]; N],
    /// Topologically sorted indices into `nodes` (dependencies before dependents;
    /// reverse iteration is the teardown order). A dependency cycle is a compile error.
    pub order: [u8; N],
    /// Elastic worker pools to register with the supervisor (empty when unused).
    #[cfg(feature = "pool")]
    pub pools: &'static [&'static dyn Pool],
}

// ─── Supervisor ──────────────────────────────────────────────────────────

/// Orchestrates a set of managed tasks across spawn / teardown / bring-up.
///
/// Owned by a single supervisor task. Concurrent access from other tasks goes
/// through each [`TaskNode`]'s own atomic state, not the `Supervisor` struct.
pub struct Supervisor<const N: usize> {
    /// Node slots, one per declared node. `None` marks a slot whose node was
    /// `#[cfg]`-ed out of the build (feature-gated); every method skips those.
    nodes: &'static [Option<&'static TaskNode>],
    /// Per-node dependency indices into `nodes` (`deps[i]` lists the indices of
    /// the nodes that node `i` depends on). The single runtime source of graph
    /// topology, generated alongside `order` by the `supervisor_graph!` macro.
    #[cfg(any(feature = "control", feature = "pool"))]
    deps: &'static [&'static [u8]],
    /// Topologically sorted indices into `nodes`: dependencies before their
    /// dependents; reverse iteration is the teardown order. Precomputed at
    /// compile time (a cycle is a compile error), so construction does no work.
    /// Borrowed from the `static` [`Graph`] rather than copied: a `Supervisor`
    /// usually lives inside a task future (i.e. in that task's `static`
    /// storage), so an inline `[u8; N]` would cost N bytes of RAM per
    /// supervisor plus the copy code for no benefit.
    order: &'static [u8; N],
    /// Elastic pools, so the control interface can co-control a whole pool from
    /// any one member (`apply_control` expands the target through
    /// [`Pool::members`]) — the same registry `run_pools` drives. Taken from
    /// `GRAPH.pools` at construction (empty when no pool is declared).
    #[cfg(feature = "pool")]
    pools: &'static [&'static dyn Pool],
}

/// Await a node's `executor:` [`SpawnerSlot`] (if it has one), bounded by the
/// node's [`slot_timeout`](TaskNode::with_slot_timeout) (default
/// [`SLOT_READY_TIMEOUT`]). A slot still empty after the wait yields
/// [`SpawnError::Busy`] — a loud misconfiguration, not a silent hang. A node with no
/// slot returns immediately, so a same-executor bring-up never touches the timer.
async fn await_spawn_slot(node: &'static TaskNode) -> Result<(), SpawnError> {
    if let Some(slot) = node.spawn_slot {
        with_timeout(node.slot_timeout, slot.ready())
            .await
            .map_err(|_| SpawnError::Busy)?;
    }
    Ok(())
}

/// Await every [`ResourceSlot`] a node's `resources:` clause takes from being
/// filled, bounded by the node's
/// [`slot_timeout`](TaskNode::with_slot_timeout) (default
/// [`SLOT_READY_TIMEOUT`]) per gate. Covers three windows: `main` providing
/// after `start` was entered; — on respawn — the previous instance's shell
/// still between the shutdown ack and its `restore()` call (on another core
/// the two can genuinely overlap); and a **provider node** still building the
/// values this node consumes (size `slot_timeout:` to the build time). A gate
/// still empty at the deadline yields [`SpawnError::Busy`] — an unprovided
/// slot is a loud misconfiguration, not a silent hang. Nodes without
/// `resources:` have an empty gate list and never touch the timer. Same
/// check-then-park loop as [`SpawnerSlot::ready`]; the `filled` signal
/// latches, so a fill racing the check still wakes the wait (and the same
/// single-pre-fill-waiter caveat applies — the supervisor task is the only
/// intended waiter).
async fn await_resources(node: &'static TaskNode) -> Result<(), SpawnError> {
    for gate in node.resource_gates {
        let wait = async {
            loop {
                if gate.is_filled() {
                    break;
                }
                gate.filled_signal().wait().await;
            }
        };
        with_timeout(node.slot_timeout, wait)
            .await
            .map_err(|_| SpawnError::Busy)?;
    }
    Ok(())
}

/// Await every `ready`-marked dep's task-asserted readiness before spawning
/// `node`, each bounded by the node's `slot_timeout` (same budget as its
/// resource gates — both are "my inputs aren't there yet"). Timeout maps to
/// `SpawnError::Busy` like the other pre-spawn gates; the log line names the
/// dep so a readiness timeout is distinguishable from a slot timeout.
#[cfg(feature = "readiness")]
async fn await_ready_deps(node: &'static TaskNode) -> Result<(), SpawnError> {
    for dep in node.ready_deps {
        if with_timeout(node.slot_timeout, dep.wait_ready())
            .await
            .is_err()
        {
            warn!(
                "supervisor: ready-dep {} not ready within {}ms (spawning {})",
                dep.name,
                node.slot_timeout.as_millis(),
                node.name,
            );
            return Err(SpawnError::Busy);
        }
    }
    Ok(())
}
#[cfg(not(feature = "readiness"))]
async fn await_ready_deps(_node: &'static TaskNode) -> Result<(), SpawnError> {
    Ok(())
}

impl<const N: usize> Supervisor<N> {
    /// Build a supervisor from a precomputed [`Graph`] — the `GRAPH` that
    /// `supervisor_graph!` emits (node slots, dependency-index table, compile-time
    /// topological `order`, and the elastic pools). A dependency cycle is a
    /// *compile* error, so construction is infallible and does no work —
    /// `start` / `teardown` / `respawn_terminate` just iterate.
    pub const fn new(graph: &'static Graph<N>) -> Self {
        Self {
            nodes: graph.nodes,
            #[cfg(any(feature = "control", feature = "pool"))]
            deps: graph.deps,
            order: &graph.order,
            #[cfg(feature = "pool")]
            pools: graph.pools,
        }
    }

    /// Bring the graph from any quiescent state to running, in dependency
    /// order — cold boot AND re-entry (a sub-graph supervisor is legitimately
    /// `start()`/`teardown()`-cycled per app phase). Idempotent: running nodes
    /// are skipped; detached nodes are skipped on re-entry (their instance
    /// survived the teardown — the first start still spawns them, the flag is
    /// app-set afterwards); a `Pause` instance parked by an earlier teardown is
    /// **resumed in place** (never double-spawned; like
    /// [`resume_pausable`](Self::resume_pausable) this bypasses the gate waits,
    /// since the parked instance retains its resources and its slots are empty
    /// by design). `Mode::OnDemand` nodes are skipped — they're brought up at
    /// runtime by `start_node`. A **parked** node (no `spawn` fn) is spawned
    /// externally by `main()` (with hardware handles main owns); it's still
    /// marked `running` here. Disabled nodes, and `#[cfg]`-ed-out slots, are
    /// skipped.
    ///
    /// Async because an `executor: NAME` node first awaits its [`SpawnerSlot::ready`]
    /// (bounded by `SLOT_READY_TIMEOUT` — the rendezvous with a tier or second core
    /// that comes up asynchronously); a slot still empty at the deadline fails the
    /// bring-up with [`SpawnError::Busy`]. A node with no `executor:` slot never
    /// touches the timer.
    pub async fn start(&self, spawner: Spawner) -> Result<(), SpawnError> {
        // Register the node slots with the trace recorders.
        #[cfg(feature = "trace")]
        trace::register_graph(self.nodes);

        for i in self.order.iter() {
            let Some(node) = self.nodes[*i as usize] else {
                continue;
            };
            if matches!(node.mode, Mode::OnDemand) || node.is_disabled() {
                continue;
            }
            // Re-entry guards, making start() the universal quiescent-to-running
            // op (cold boot, post-teardown cycle, partial states) — all three
            // are no-ops on a cold boot:
            // * already running -> skip (idempotent; trustworthy because a
            //   cleanly returned body clears `running` via mark_exited);
            // * detached -> skip (its instance survived the teardown that
            //   preceded this start; spawning again would double-spawn — the
            //   flag is app-set at runtime, so first-start still spawns it);
            // * a Pause instance parked by an earlier teardown -> resume it in
            //   place below, never spawn a second one.
            if node.is_running() || node.is_detached() {
                continue;
            }
            if matches!(node.mode, Mode::Pause) && node.has_acked_stop() {
                // Same sequence as resume_pausable, and like it deliberately
                // WITHOUT the spawn path's gate waits: the parked instance
                // retains its resources, so its slots are empty by design and
                // await_resources would time out Busy.
                node.reset();
                info!("supervisor: resuming {} in place", node.name);
                node.signal_resume();
                node.set_running(true);
                continue;
            }
            // Clean handle per cycle (like start_node): a sub-graph supervisor
            // is legitimately start()/teardown()-cycled per app phase, and the
            // teardown latches the shutdown flag — without this reset a second
            // start()'s workers would observe it instantly. No-op at boot.
            node.reset();
            info!("supervisor: spawning {} ({})", node.name, node.mode);
            if let Some(spawn) = node.spawn {
                // For an `executor:` node, wait (bounded) for its slot to be filled
                // before spawning; a same-executor node has no slot, so this is an
                // immediate no-op and the bring-up loop stays tight. Then wait for
                // the node's `resources:` slots (if any) so the glue's take() finds
                // the value even if main provides late.
                await_spawn_slot(node).await?;
                await_resources(node).await?;
                await_ready_deps(node).await?;
                spawn(spawner)?;
            }
            node.set_running(true);
        }
        Ok(())
    }

    /// The canonical driver, as one call: [`start`](Self::start) the graph,
    /// then drive elastic-pool scaling and/or runtime control forever. Returns
    /// **only on error** — every arm is an app-level escalation (typically
    /// `panic!` into a hardware-watchdog reset):
    ///
    /// ```ignore
    /// match sup.run(spawner).await {
    ///     RunError::Spawn(_) => defmt::panic!("supervisor: bring-up failed"),
    ///     RunError::Shutdown(e) => defmt::panic!("supervisor: {} missed ack", e.node.name),
    /// }
    /// ```
    ///
    /// Apps that select extra wake sources into the driver loop (their own
    /// signals, a wake timer) keep writing the loop by hand:
    /// `select(sup.run_pools(spawner), wait_control())` + `apply_control`.
    #[cfg(any(feature = "pool", feature = "control"))]
    pub async fn run(&self, spawner: Spawner) -> RunError {
        if let Err(e) = self.start(spawner).await {
            return RunError::Spawn(e);
        }
        #[cfg(all(feature = "pool", feature = "control"))]
        loop {
            match select(self.run_pools(spawner), wait_control()).await {
                Either::First(e) => return RunError::Shutdown(e),
                Either::Second(cmd) => {
                    if let Err(e) = self.apply_control(cmd, spawner).await {
                        return RunError::Shutdown(e);
                    }
                }
            }
        }
        #[cfg(all(feature = "pool", not(feature = "control")))]
        return RunError::Shutdown(self.run_pools(spawner).await);
        #[cfg(all(feature = "control", not(feature = "pool")))]
        loop {
            let cmd = wait_control().await;
            if let Err(e) = self.apply_control(cmd, spawner).await {
                return RunError::Shutdown(e);
            }
        }
    }

    /// Start a single node at runtime — e.g. growing an elastic pool. Resets the
    /// handle, spawns one instance via the node's `spawn` fn (which must launch
    /// exactly one), and marks it `running`. Returns `SpawnError::Busy` if the
    /// underlying embassy task pool is exhausted (the ceiling), which the caller
    /// treats as "can't grow".
    pub async fn start_node(
        &self,
        node: &'static TaskNode,
        spawner: Spawner,
    ) -> Result<(), SpawnError> {
        node.reset();
        if let Some(spawn) = node.spawn {
            await_spawn_slot(node).await?;
            await_resources(node).await?;
            await_ready_deps(node).await?;
            spawn(spawner)?;
        }
        node.set_running(true);
        info!("supervisor: started {}", node.name);
        Ok(())
    }

    /// Signal `node` to shut down, wait for its ack, then clear `running`.
    /// A missed ack (a missing `ack_dropped()`/`mark_exited()` somewhere, or a
    /// wedged task) is returned as [`ShutdownTimeout`] — the node keeps running
    /// and the caller decides the escalation. Shared by `stop_node` and
    /// `teardown`; the caller must have checked `is_running`.
    async fn shutdown_and_wait(&self, node: &'static TaskNode) -> Result<(), ShutdownTimeout> {
        node.signal_shutdown();
        if let Either::Second(()) = select(
            node.wait_dropped(),
            Timer::after_millis(SHUTDOWN_ACK_TIMEOUT_MS),
        )
        .await
        {
            warn!(
                "supervisor: task {} did not ack shutdown within {}ms",
                node.name, SHUTDOWN_ACK_TIMEOUT_MS,
            );
            return Err(ShutdownTimeout { node });
        }
        node.set_running(false);
        Ok(())
    }

    /// Stop a single running node at runtime — e.g. shrinking an elastic pool.
    /// For a `Pause` node this IS the single-node "pause": the worker acks and
    /// parks on `wait_resume()`, and [`resume_node`](Self::resume_node) is the
    /// symmetric other half. Signals shutdown, waits for the ack, clears
    /// `running`. No-op `Ok` if the node isn't running, or is
    /// [detached](TaskNode::set_detached) (self-managed — the supervisor never
    /// stops it). A node that misses the ack window is returned as
    /// [`ShutdownTimeout`] and stays marked running.
    pub async fn stop_node(&self, node: &'static TaskNode) -> Result<(), ShutdownTimeout> {
        if !node.is_running() || node.is_detached() {
            return Ok(());
        }
        self.shutdown_and_wait(node).await?;
        info!("supervisor: stopped {}", node.name);
        Ok(())
    }

    /// Signal every **running** node to shut down in **reverse** topological
    /// order, awaiting each node's ack before moving to its dependency. Down
    /// `OnDemand` nodes are skipped (no instance to ack). Pause-mode nodes ack
    /// and park on `wait_resume()`; Terminate/OnDemand nodes exit.
    ///
    /// **Aborts on the first missed ack**, returning the offending node as
    /// [`ShutdownTimeout`]: continuing would stop dependencies out from under a
    /// still-live dependent. After `Err` the graph is partially down — the sane
    /// escalations are app-level (hardware watchdog reset, `panic!`, retry, or
    /// [`teardown_continue`](Self::teardown_continue) when quiescing the rest
    /// still matters before a reset).
    pub async fn teardown(&self) -> Result<(), ShutdownTimeout> {
        for i in self.order.iter().rev() {
            let Some(node) = self.nodes[*i as usize] else {
                continue;
            };
            if !node.is_running() {
                continue;
            }
            // A detached node is self-managed; never tear it down. See
            // [`TaskNode::set_detached`].
            if node.is_detached() {
                continue;
            }
            info!("supervisor: tearing down {}", node.name);
            self.shutdown_and_wait(node).await?;
        }
        Ok(())
    }

    /// Best-effort variant of [`teardown`](Self::teardown) for the
    /// "hardware reset next" escalation path: presses on past a non-acking node
    /// (still in reverse topological order) so the remaining nodes get their
    /// chance to flush and park, and returns the **first** timeout after
    /// visiting every node. The wedged node's dependencies are stopped under it
    /// — acceptable only because the caller is about to reset anyway.
    pub async fn teardown_continue(&self) -> Result<(), ShutdownTimeout> {
        let mut first_err = Ok(());
        for i in self.order.iter().rev() {
            let Some(node) = self.nodes[*i as usize] else {
                continue;
            };
            if !node.is_running() || node.is_detached() {
                continue;
            }
            info!("supervisor: tearing down {}", node.name);
            if let Err(e) = self.shutdown_and_wait(node).await {
                if first_err.is_ok() {
                    first_err = Err(e);
                }
            }
        }
        first_err
    }

    /// Resume ONE `Pause` node parked by an earlier [`stop_node`](Self::stop_node)
    /// or [`teardown`](Self::teardown) — the single-node partner of
    /// [`resume_pausable`](Self::resume_pausable), same sequence and the same
    /// deliberate absence of dependency gating (the parked instance retains its
    /// resources). Cheap and synchronous. No-op unless the node is `Pause`
    /// mode, actually parked (an instance acked without exiting), and neither
    /// [disabled](TaskNode::is_disabled) (a control pause sticks — clear it
    /// with [`activate`](Self::activate)) nor
    /// [detached](TaskNode::set_detached).
    pub fn resume_node(&self, node: &'static TaskNode) {
        if !matches!(node.mode, Mode::Pause)
            || node.is_disabled()
            || node.is_detached()
            || !node.has_acked_stop()
        {
            return;
        }
        node.reset();
        info!("supervisor: resuming {}", node.name);
        node.signal_resume();
        node.set_running(true);
    }

    /// Signal every Pause-mode node to resume. Cheap and synchronous — the tasks
    /// were parked on `wait_resume()` and pick up immediately. Called separately
    /// from `respawn_terminate` so the application can fire resume independently
    /// of the respawn step. Disabled (manually-paused) nodes are skipped so a
    /// manual pause sticks, and detached (self-managed) Pause nodes are left
    /// parked; there is intentionally no dependency gate here.
    pub fn resume_pausable(&self) {
        for i in self.order.iter() {
            let Some(node) = self.nodes[*i as usize] else {
                continue;
            };
            if matches!(node.mode, Mode::Pause) && !node.is_disabled() && !node.is_detached() {
                node.reset();
                info!("supervisor: resuming {}", node.name);
                node.signal_resume();
                node.set_running(true);
            }
        }
    }

    /// Reset and re-spawn every Terminate-mode node in dependency order.
    /// Pause-mode nodes are untouched (use `resume_pausable`); `OnDemand` nodes
    /// are left down — they re-grow under load via `start_node`. Disabled nodes
    /// are skipped so a manual stop sticks across the bring-up. Detached nodes are
    /// skipped too: `teardown` never brought them down, so they are still running
    /// and re-spawning would double-spawn them (see [`TaskNode::set_detached`]). The
    /// reset happens before the spawn so newly-running tasks see a clean handle.
    pub async fn respawn_terminate(&self, spawner: Spawner) -> Result<(), SpawnError> {
        for i in self.order.iter() {
            let Some(node) = self.nodes[*i as usize] else {
                continue;
            };
            if matches!(node.mode, Mode::Terminate) && !node.is_disabled() && !node.is_detached() {
                node.reset();
                info!("supervisor: respawning {}", node.name);
                if let Some(spawn) = node.spawn {
                    await_spawn_slot(node).await?;
                    // A `resources:` node's previous instance restores its slot
                    // value only after the shutdown ack, so wait (bounded) for
                    // the restore before the glue's take().
                    await_resources(node).await?;
                    await_ready_deps(node).await?;
                    spawn(spawner)?;
                }
                node.set_running(true);
            }
        }
        Ok(())
    }
}

// ─── Runtime control (dependency- and pool-honoring start/stop) ────────────
//
// The `apply_control` entry point drives one `ControlCommand` from the
// application's control surface. Unlike the pool's bare `start_node`/`stop_node`,
// these honor the graph: a stop cascades through dependents (so nothing is left
// running without a dependency), a start cascades through deps (so nothing comes
// up before what it needs), and either expands across a whole `ElasticPool` so
// the pool is controlled as a unit. A manual stop/pause also sets the
// lifecycle-spanning `disabled` flag, so it sticks against the elastic policy and
// the wake respawn.

// Graph-index helpers used by BOTH the control plane and the pool driver, so they
// are gated on either feature — `pool` alone (no `control`) must still compile.
#[cfg(any(feature = "control", feature = "pool"))]
impl<const N: usize> Supervisor<N> {
    /// Position of `node` in `self.nodes` (pointer identity — every node is a
    /// `&'static`). `None` only if the node isn't in this graph (impossible for
    /// targets sourced from `GRAPH.nodes`; treated as a no-op by callers).
    fn index_of(&self, node: &'static TaskNode) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| n.is_some_and(|x| core::ptr::eq(x, node)))
    }

    /// Whether every dependency of `node` is currently running, resolved through
    /// the graph's index table. The pool driver checks this before growing a
    /// worker, so a pool member is never spawned while one of its dependencies is
    /// down.
    #[cfg(feature = "pool")]
    pub(crate) fn deps_running(&self, node: &'static TaskNode) -> bool {
        match self.index_of(node) {
            Some(i) => self.deps[i]
                .iter()
                .all(|&di| self.nodes[di as usize].is_some_and(|n| n.is_running())),
            None => false,
        }
    }
}

#[cfg(feature = "control")]
impl<const N: usize> Supervisor<N> {
    /// Seed a membership set with `target` plus — if `target` belongs to an
    /// elastic pool — every member of that pool, so control is applied to the
    /// whole pool atomically. Pool membership is read from `GRAPH.pools`; with no
    /// pools (the `pool` feature off, or none declared) this is just `{target}`.
    fn seed(&self, target: &'static TaskNode, set: &mut [bool; N]) {
        if let Some(i) = self.index_of(target) {
            set[i] = true;
        }
        #[cfg(feature = "pool")]
        for pool in self.pools {
            let members = pool.members();
            if members.iter().any(|m| core::ptr::eq(*m, target)) {
                for m in members {
                    if let Some(i) = self.index_of(m) {
                        set[i] = true;
                    }
                }
            }
        }
    }

    /// Apply one control command, honoring pool membership and the dependency
    /// graph — the mailbox-dispatch form of [`activate`](Self::activate) /
    /// [`deactivate`](Self::deactivate) (call those directly when you hold the
    /// supervisor). Run from the supervisor's driver loop (never concurrently
    /// with itself), so the cascade is atomic from the application's
    /// perspective. A `Deactivate` cascade propagates a missed shutdown ack as
    /// [`ShutdownTimeout`] (the cascade aborts at the offending node, dependents
    /// already stopped); `Activate` cannot fail this way.
    pub async fn apply_control(
        &self,
        cmd: ControlCommand,
        spawner: Spawner,
    ) -> Result<(), ShutdownTimeout> {
        match cmd.op {
            ControlOp::Deactivate => self.deactivate(cmd.node).await,
            ControlOp::Activate => {
                self.activate(cmd.node, spawner).await;
                Ok(())
            }
        }
    }

    /// Bring `target` (and its pool, and every transitive dependent) down, in
    /// reverse-topological order so each dependent stops before the dependency it
    /// relies on — the cascading "turn this subsystem off" verb, and the exit
    /// half of the subordinate sub-graph pattern's one-graph variant. Marks
    /// the whole set `disabled` so the stop sticks against the elastic policy
    /// and the wake respawn until a matching [`activate`](Self::activate).
    /// Aborts with [`ShutdownTimeout`] on a missed ack (the offending node stays
    /// running and disabled; dependents visited before it are already down).
    ///
    /// Contrast [`stop_node`](Self::stop_node): ONE node, no cascade, no
    /// `disabled` latch (the pool-shrink primitive). Call this directly when
    /// you hold the supervisor; [`request_control`] +
    /// [`apply_control`](Self::apply_control) is the same operation routed
    /// through the mailbox from code that doesn't.
    pub async fn deactivate(&self, target: &'static TaskNode) -> Result<(), ShutdownTimeout> {
        let mut set = [false; N];
        self.seed(target, &mut set);

        // Grow the set to include transitive dependents. `order` is
        // dependency-first, so when we reach a node its deps are already decided;
        // a node joins if any dep it declares is already in the set.
        for i in self.order.iter() {
            let j = *i as usize;
            if set[j] {
                continue;
            }
            let Some(node) = self.nodes[j] else {
                continue;
            };
            // A detached node declares its dep only for start ordering and intends
            // to outlive it, so it's never pulled into the cascade.
            if node.is_detached() {
                continue;
            }
            if self.deps[j].iter().any(|&di| set[di as usize]) {
                set[j] = true;
            }
        }

        // Tear down in reverse topo order (dependents before their deps).
        for i in self.order.iter().rev() {
            let j = *i as usize;
            if !set[j] {
                continue;
            }
            let Some(node) = self.nodes[j] else {
                continue;
            };
            // A detached node is self-managed — never control-stop it. The growth loop
            // keeps detached *dependents* out of the set; this also covers a detached
            // node that was seeded directly (or a detached pool member). Without it a
            // detached one-shot that already exited (stale `is_running`, no ack path)
            // would be signalled a shutdown it can never acknowledge, failing here
            // with a spurious `ShutdownTimeout`.
            if node.is_detached() {
                continue;
            }
            node.set_disabled(true);
            if node.is_running() {
                info!("supervisor: control-stop {}", node.name);
                self.shutdown_and_wait(node).await?;
            }
        }
        Ok(())
    }

    /// Bring `target` (and its pool, and every transitive dependency) up, in
    /// topological order so each dependency starts before its dependent — the
    /// cascading "turn this subsystem on" verb, and the entry half of the
    /// subordinate sub-graph pattern's one-graph variant: `activate` on a
    /// subtree's LEAF pulls its whole dependency chain up, skipping
    /// already-running nodes. Per-node spawn errors are deliberately swallowed
    /// (a cascade is best-effort; a `Busy` member is re-driven by the pool
    /// policy or a later activate), so this returns `()` — asymmetric with
    /// [`deactivate`](Self::deactivate) on purpose. Clears
    /// `disabled` across the set. `OnDemand` (pool) members are only re-enabled,
    /// not force-spawned — the elastic policy re-grows them under load, which is
    /// the whole point of the pool.
    pub async fn activate(&self, target: &'static TaskNode, spawner: Spawner) {
        let mut set = [false; N];
        self.seed(target, &mut set);

        // Grow the set to include transitive deps. Walk dependents-first
        // (reverse topo); when a set member is seen, pull in its direct deps.
        // A detached member's `deps:` are start-ordering only (the node is
        // self-managed), so don't expand from it — mirrors deactivate's guard;
        // otherwise activating a detached target would un-disable deps that
        // were independently disabled.
        for i in self.order.iter().rev() {
            let j = *i as usize;
            if set[j] && !self.nodes[j].is_some_and(|n| n.is_detached()) {
                for &di in self.deps[j] {
                    set[di as usize] = true;
                }
            }
        }

        // Bring up in topo order (deps before dependents).
        for i in self.order.iter() {
            let j = *i as usize;
            if !set[j] {
                continue;
            }
            let Some(node) = self.nodes[j] else {
                continue;
            };
            // A detached node is self-managed — the supervisor never re-enables or
            // re-starts it, even when it is a dependency of an activated target.
            if node.is_detached() {
                continue;
            }
            node.set_disabled(false);
            if node.is_running() {
                continue;
            }
            match node.mode {
                Mode::Terminate => {
                    info!("supervisor: control-start {}", node.name);
                    // SpawnError::Busy (pool exhausted) → can't start, skip.
                    let _ = self.start_node(node, spawner).await;
                }
                Mode::Pause => {
                    info!("supervisor: control-resume {}", node.name);
                    node.reset();
                    node.signal_resume();
                    node.set_running(true);
                }
                // Pool worker — leave it down; the elastic policy regrows it on
                // demand now that `disabled` is cleared.
                Mode::OnDemand => {}
            }
        }
    }
}

// ─── Topological sort (Kahn's algorithm, const) ───────────────────────────
//
// Computes the topological order at *compile time* over a per-node
// dependency-index table; a dependency cycle is a compile error.

/// Topologically sort a graph given as a per-node dependency-index table.
///
/// `deps[i]` lists the indices of the nodes that node `i` depends on; the result
/// lists node indices in dependency-first order (a dependency appears before its
/// dependents). The supervisor iterates it forward for `start` /
/// `respawn_terminate` and in reverse for `teardown`.
///
/// Evaluated at compile time by the code `supervisor_graph!` generates — a
/// dependency **cycle is a compile error** (the `panic!` fires during const
/// evaluation). `#[doc(hidden)]`: an engine for the macro, not a user-facing API.
///
/// Supports at most 256 nodes: indices are `u8`, so a larger `N` would truncate.
/// The macro rejects bigger graphs at expansion; the assert below is defense in
/// depth for a manual caller (a const-eval panic, i.e. a compile error).
#[doc(hidden)]
#[must_use]
pub const fn topo_sort_const<const N: usize>(deps: &[&'static [u8]; N]) -> [u8; N] {
    assert!(
        N <= 256,
        "supervisor graph exceeds 256 node slots (indices are u8)"
    );
    // in_degree[i] = number of deps of node i not yet resolved.
    let mut in_degree = [0u8; N];
    let mut i = 0;
    while i < N {
        in_degree[i] = deps[i].len() as u8;
        i += 1;
    }

    // Queue (fixed array, head/tail indices) seeded with the dependency-free nodes.
    let mut queue = [0u8; N];
    let mut tail = 0;
    i = 0;
    while i < N {
        if in_degree[i] == 0 {
            queue[tail] = i as u8;
            tail += 1;
        }
        i += 1;
    }

    let mut order = [0u8; N];
    let mut produced = 0;
    let mut head = 0;
    while head < tail {
        let node = queue[head] as usize;
        head += 1;
        order[produced] = node as u8;
        produced += 1;

        // Decrement the in-degree of every node that depends on `node`.
        let mut j = 0;
        while j < N {
            if in_degree[j] != 0 {
                let mut depends = false;
                let mut k = 0;
                while k < deps[j].len() {
                    if deps[j][k] as usize == node {
                        depends = true;
                    }
                    k += 1;
                }
                if depends {
                    in_degree[j] -= 1;
                    if in_degree[j] == 0 {
                        queue[tail] = j as u8;
                        tail += 1;
                    }
                }
            }
            j += 1;
        }
    }

    // A cycle leaves some nodes unproduced. During const eval this panic is a
    // compile error, so cyclic graphs are rejected at build time. `core::panic!`
    // (not the crate's defmt-shimmed `panic!`) keeps this const-evaluable.
    if produced != N {
        core::panic!("supervisor_graph!: dependency cycle");
    }
    order
}

#[cfg(feature = "pool")]
mod pool;
#[cfg(feature = "pool")]
pub use pool::*;

#[cfg(feature = "trace")]
pub mod trace;

#[cfg(feature = "macros")]
pub use embassy_supervisor_macros::supervisor_fragment;
/// Declare a supervised task graph and compute its topological order at compile
/// time (single source of nodes, deps, pool, and order). See the
/// `embassy-supervisor-macros` crate for the surface syntax.
#[cfg(feature = "macros")]
pub use embassy_supervisor_macros::supervisor_graph;

/// Assemble one graph from `supervisor_fragment!` relays plus compose-site
/// items:
///
/// ```ignore
/// embassy_supervisor::compose_graph! {
///     fragments: [::net_stack::NET_FRAG, HTTP_FRAG],
///     graph: {
///         node APP = Terminate, deps: [NET], task: app_worker; // cross-fragment dep
///     }
/// }
/// ```
///
/// Fragments expand in listed order, then the `graph:` items; everything
/// reaches ONE `supervisor_graph!` expansion, so cross-fragment deps resolve by
/// name (forward references included) and every compile-time pass — name map,
/// u8 slot indices, topological order, shared-slot dedup, the 256-node cap —
/// checks the whole composed graph. One compose site per binary (it emits the
/// usual `GRAPH`/`NODES`/`DEPS` statics and, under `trace-hooks`, the hook
/// symbols). Name collisions across fragments hit the ordinary duplicate-name
/// errors, attributed to the owning fragment; prefix fragment-public names.
#[cfg(feature = "macros")]
#[macro_export]
macro_rules! compose_graph {
    // `name: X,` first renames the composed graph static (see
    // `supervisor_graph!`'s `name:`) — seeded into the accumulator ahead of
    // every fragment's items so it stays the expansion's first item.
    (name: $n:ident, fragments: [$f:path $(, $r:path)* $(,)?], graph: {$($g:tt)*}) => {
        $f! { @emit $crate::compose_graph, [$($r),*], {name: $n;}, {$($g)*} }
    };
    (fragments: [$f:path $(, $r:path)* $(,)?], graph: {$($g:tt)*}) => {
        $f! { @emit $crate::compose_graph, [$($r),*], {}, {$($g)*} }
    };
    (@next [], {$($acc:tt)*}, {$($g:tt)*}) => {
        $crate::supervisor_graph! { $($acc)* $($g)* }
    };
    (@next [$f:path $(, $r:path)*], {$($acc:tt)*}, $g:tt) => {
        $f! { @emit $crate::compose_graph, [$($r),*], {$($acc)*}, $g }
    };
}

/// Building blocks for `supervisor_graph!`-generated code — NOT public API.
///
/// The macro's `local`-marked `resources:` entries emit a slot *type* at the
/// graph declaration site (it needs an `unsafe impl Sync`, — same reason the
/// `trace-hooks` symbols are emitted there). That generated type must name
/// the exact `Signal`/mutex types in [`ResourceGate`]'s signature; re-exporting
/// them here keeps the macro's contract that a consumer only needs
/// `embassy-supervisor` itself as a real-named dependency (not `embassy-sync`).
#[doc(hidden)]
pub mod _export {
    pub use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
    pub use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    pub use embassy_sync::signal::Signal;
    // For the `slot_timeout:` clause's emitted `with_slot_timeout(..)` call.
    pub use embassy_time::Duration;
}
