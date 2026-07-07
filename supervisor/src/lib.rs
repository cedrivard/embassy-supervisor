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
//!   * `resources: [NAME: Type, ..]` on a `task:` node threads **owned resources
//!     from `main`** into the worker through macro-emitted [`ResourceSlot`]s —
//!     compile-time exclusive ownership (the `Peripherals` field is consumed, no
//!     `steal()` inside the task), fail-closed provisioning (an unprovided slot
//!     fails `start` with `SpawnError::Busy`), and restore-on-exit so a respawn
//!     re-takes the *same instance*.
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
//! A supervised task is a plain `#[embassy_executor::task]` whose first parameter
//! is its node (the macro's `spawn:` glue passes it; extra arguments come from the
//! partial-call spawn form). Four rules cover the task side of the protocol:
//!
//!   1. select long-lived work against [`TaskNode::wait_shutdown`] — that's how a
//!      stop reaches you;
//!   2. ack exactly once per stop with [`TaskNode::ack_dropped`]: on exit
//!      (`Terminate`/`OnDemand`), or on each pause (`Pause`) *before* parking on
//!      [`TaskNode::wait_resume`];
//!   3. an autonomous exit acks too, so the supervisor sees the node as down;
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
//! ## What the supervisor does *not* do
//!
//!   * It does not model any power-state transition (sleep/wake): it reacts to
//!     "teardown" and "bring-up" requests; the application drives them.
//!   * It does not allocate, and does no work at construction: the topological
//!     sort runs at compile time (see the `supervisor_graph!` macro).
//!   * It does not observe task internals. Tasks self-report their drop state via
//!     `ack_dropped()`; a task that fails to ack within a timeout panics the
//!     supervisor with the offending node's name.
//!
//! ## Cargo features
//!
//!   * `control` *(default)* — the runtime control plane: [`ControlOp`],
//!     [`request_control`], [`Supervisor::apply_control`].
//!   * `pool` *(default)* — elastic worker pools: [`ElasticPool`],
//!     [`Supervisor::run_pools`], and the `pools` field of [`Graph`].
//!   * `defmt` — route the supervisor's logs through `defmt`; without it the log
//!     macros are no-ops.
//!   * `trace` family (all opt-in) — `trace`: the [`trace`] recorders consuming
//!     embassy-executor's `_embassy_trace_*` hooks; `trace-hooks`:
//!     `supervisor_graph!` also *defines* the hook symbols; `trace-names`: node
//!     names stamped into task Metadata for external consumers; `trace-nested`:
//!     preemption-exact accounting (a nested higher-tier poll credits its time
//!     back to the window it interrupted).
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
//! use embassy_supervisor::{supervisor_graph, Supervisor, wait_control};
//!
//! // `app` depends on `net`; each `spawn:` names a task fn spawned with the node.
//! supervisor_graph! {
//!     node NET = Terminate, deps: [], spawn: net_task;
//!     node APP = Terminate, deps: [NET], spawn: app_task;
//! }
//!
//! #[embassy_executor::task]
//! async fn supervisor_task(spawner: Spawner) {
//!     let sup = Supervisor::new(&GRAPH);
//!     sup.start(spawner).await.expect("initial spawn"); // brings up `net`, then `app`
//!     loop {
//!         // Apply runtime start/stop/pause/resume requests in dependency order.
//!         let cmd = wait_control().await;
//!         sup.apply_control(cmd, spawner).await;
//!     }
//!     // With the `pool` feature you'd instead drive scaling and control together:
//!     // `select(sup.run_pools(spawner), wait_control())` (see the `firmware` crate).
//! }
//! ```
//!
//! The `firmware` crate in the [repository](https://github.com/cedrivard/embassy-supervisor)
//! is a complete working example (USB-net, an HTTP control plane, an elastic pool,
//! and OTA).

#[macro_use]
mod fmt;

use core::cell::Cell;
use core::sync::atomic::Ordering;

use embassy_executor::{SendSpawner, SpawnError, Spawner};
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
#[cfg(feature = "control")]
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Timer, with_timeout};
use portable_atomic::AtomicBool;
#[cfg(feature = "trace")]
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
// channel — the caller `request_control()`s a (node, op) pair and returns
// immediately; the supervisor's driver loop `wait_control()`s it and runs the
// dependency-honoring `apply_control`. A `Channel` (not a `Signal`) so
// back-to-back requests aren't coalesced; capacity 4 is ample for hand-driven
// control and a full channel simply drops the surplus (`try_send`).

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

/// Enqueue a control request. Non-blocking; drops if the mailbox is full (4
/// outstanding), which is harmless for low-frequency manual control. Called by
/// the application's control surface.
#[cfg(feature = "control")]
pub fn request_control(node: &'static TaskNode, op: ControlOp) {
    let _ = CONTROL_REQ.try_send(ControlCommand { node, op });
}

/// Await the next control request. Selected by the supervisor's driver loop
/// against pool scaling and any other application wake sources.
#[cfg(feature = "control")]
pub async fn wait_control() -> ControlCommand {
    CONTROL_REQ.receive().await
}

/// Per-node timeout for `wait_dropped`. A task that doesn't ack within this
/// window is a bug (e.g. a missing `ack_dropped()` call) and panics the
/// supervisor with the offending node's name. 2 s comfortably exceeds a typical
/// task's poll period and peripheral settle time.
const SHUTDOWN_ACK_TIMEOUT_MS: u64 = 2_000;

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
    /// Wake source for `wait_resume()` on Pause-mode tasks. Fired by
    /// `signal_resume()`.
    resume_wake: Signal<CriticalSectionRawMutex, ()>,
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
            resume_wake: Signal::new(),
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

    /// Put the resource back for the next spawn. Called by the generated task
    /// shell after the worker returns (i.e. after its clean shutdown ack), so a
    /// respawn re-takes the same instance.
    pub fn restore(&self, value: T) {
        self.provide(value);
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

    // ── Task-side API ────────────────────────────────────────────────────
    //
    // Called from inside the `#[embassy_executor::task] async fn` body. The
    // whole task-side protocol is four rules (the README's "Writing supervised
    // tasks" section has per-mode skeletons):
    //   1. select long-lived work against `wait_shutdown()`;
    //   2. `ack_dropped()` exactly once per stop — on exit (Terminate/OnDemand)
    //      or on each pause (Pause), before parking on `wait_resume()`;
    //   3. an autonomous exit acks too;
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

    /// Pause-mode only: park until the supervisor signals resume. Call *after*
    /// [`ack_dropped`](Self::ack_dropped) — ack the pause, then park; held
    /// resources stay owned across the park.
    pub async fn wait_resume(&self) {
        self.handle.resume_wake.wait().await;
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
    /// the task id for the [`trace`] recorders and (feature `trace-names`)
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
        #[cfg(feature = "trace-names")]
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
    pub(crate) async fn wait_dropped(&self) {
        if self.handle.dropped.load(Ordering::Acquire) {
            return;
        }
        self.handle.dropped_wake.wait().await;
    }

    /// Clear the shutdown flag, dropped flag, busy flag, and the shutdown /
    /// dropped wake-signals so the next cycle starts clean. Doesn't touch
    /// `running` (managed around spawn/stop), `resume_wake` (`resume_pausable`
    /// fires that for Pause nodes), or `disabled` (lifecycle-spanning).
    pub(crate) fn reset(&self) {
        self.handle.shutdown.store(false, Ordering::Release);
        self.handle.dropped.store(false, Ordering::Release);
        self.handle.busy.store(false, Ordering::Release);
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
    deps: &'static [&'static [u8]],
    /// Topologically sorted indices into `nodes`: dependencies before their
    /// dependents; reverse iteration is the teardown order. Precomputed at
    /// compile time (a cycle is a compile error), so construction does no work.
    order: [u8; N],
    /// Elastic pools, so the control interface can co-control a whole pool from
    /// any one member (`apply_control` expands the target through
    /// [`Pool::members`]) — the same registry `run_pools` drives. Taken from
    /// `GRAPH.pools` at construction (empty when no pool is declared).
    #[cfg(feature = "pool")]
    pools: &'static [&'static dyn Pool],
}

/// Await a node's `executor:` [`SpawnerSlot`] (if it has one), bounded by
/// [`SLOT_READY_TIMEOUT`]. A slot still empty after the wait yields
/// [`SpawnError::Busy`] — a loud misconfiguration, not a silent hang. A node with no
/// slot returns immediately, so a same-executor bring-up never touches the timer.
async fn await_spawn_slot(node: &'static TaskNode) -> Result<(), SpawnError> {
    if let Some(slot) = node.spawn_slot {
        with_timeout(SLOT_READY_TIMEOUT, slot.ready())
            .await
            .map_err(|_| SpawnError::Busy)?;
    }
    Ok(())
}

/// Await every [`ResourceSlot`] a node's `resources:` clause takes from being
/// filled, bounded by [`SLOT_READY_TIMEOUT`] per gate. Covers two windows:
/// `main` providing after `start` was entered, and — on respawn — the previous
/// instance's shell still between the shutdown ack and its `restore()` call
/// (on another core the two can genuinely overlap). A gate still empty at the
/// deadline yields [`SpawnError::Busy`] — an unprovided slot is a loud
/// misconfiguration, not a silent hang. Nodes without `resources:` have an
/// empty gate list and never touch the timer. Same check-then-park loop as
/// [`SpawnerSlot::ready`]; the `filled` signal latches, so a fill racing the
/// check still wakes the wait (and the same single-pre-fill-waiter caveat
/// applies — the supervisor task is the only intended waiter).
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
        with_timeout(SLOT_READY_TIMEOUT, wait)
            .await
            .map_err(|_| SpawnError::Busy)?;
    }
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
            deps: graph.deps,
            order: graph.order,
            #[cfg(feature = "pool")]
            pools: graph.pools,
        }
    }

    /// Spawn every boot node in dependency order. Called once at boot.
    /// `Mode::OnDemand` nodes are skipped — they're brought up at runtime by
    /// `start_node`. A **parked** node (no `spawn` fn) is spawned externally by
    /// `main()` (with hardware handles main owns); it's still marked `running`
    /// here. Disabled nodes, and `#[cfg]`-ed-out slots, are skipped.
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
            info!("supervisor: spawning {} ({})", node.name, node.mode);
            if let Some(spawn) = node.spawn {
                // For an `executor:` node, wait (bounded) for its slot to be filled
                // before spawning; a same-executor node has no slot, so this is an
                // immediate no-op and the bring-up loop stays tight. Then wait for
                // the node's `resources:` slots (if any) so the glue's take() finds
                // the value even if main provides late.
                await_spawn_slot(node).await?;
                await_resources(node).await?;
                spawn(spawner)?;
            }
            node.set_running(true);
        }
        Ok(())
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
            spawn(spawner)?;
        }
        node.set_running(true);
        info!("supervisor: started {}", node.name);
        Ok(())
    }

    /// Signal `node` to shut down, wait for its ack (panicking on timeout — a
    /// missing `ack_dropped()` somewhere), then clear `running`. Shared by
    /// `stop_node` and `teardown`; the caller must have checked `is_running`.
    async fn shutdown_and_wait(&self, node: &'static TaskNode) {
        node.signal_shutdown();
        if let Either::Second(()) = select(
            node.wait_dropped(),
            Timer::after_millis(SHUTDOWN_ACK_TIMEOUT_MS),
        )
        .await
        {
            panic!(
                "supervisor: task {} did not ack shutdown within {}ms",
                node.name, SHUTDOWN_ACK_TIMEOUT_MS,
            );
        }
        node.set_running(false);
    }

    /// Stop a single running node at runtime — e.g. shrinking an elastic pool.
    /// Signals shutdown, waits for the ack, clears `running`. No-op if the node
    /// isn't running, or is [detached](TaskNode::set_detached) (self-managed — the
    /// supervisor never stops it). Panics if it doesn't ack within the timeout.
    pub async fn stop_node(&self, node: &'static TaskNode) {
        if !node.is_running() || node.is_detached() {
            return;
        }
        self.shutdown_and_wait(node).await;
        info!("supervisor: stopped {}", node.name);
    }

    /// Signal every **running** node to shut down in **reverse** topological
    /// order, awaiting each node's ack before moving to its dependency. Down
    /// `OnDemand` nodes are skipped (no instance to ack). Pause-mode nodes ack
    /// and park on `wait_resume()`; Terminate/OnDemand nodes exit. Panics if a
    /// running node fails to ack within `SHUTDOWN_ACK_TIMEOUT_MS`.
    pub async fn teardown(&self) {
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
            self.shutdown_and_wait(node).await;
        }
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
    /// graph. Run from the supervisor's driver loop (never concurrently with
    /// itself), so the cascade is atomic from the application's perspective.
    pub async fn apply_control(&self, cmd: ControlCommand, spawner: Spawner) {
        match cmd.op {
            ControlOp::Deactivate => self.deactivate(cmd.node).await,
            ControlOp::Activate => self.activate(cmd.node, spawner).await,
        }
    }

    /// Bring `target` (and its pool, and every transitive dependent) down, in
    /// reverse-topological order so each dependent stops before the dependency it
    /// relies on. Marks the whole set `disabled` so the stop sticks against the
    /// elastic policy and the wake respawn until a matching `activate`.
    async fn deactivate(&self, target: &'static TaskNode) {
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
            // would be signalled a shutdown it can never acknowledge, panicking here.
            if node.is_detached() {
                continue;
            }
            node.set_disabled(true);
            if node.is_running() {
                info!("supervisor: control-stop {}", node.name);
                self.shutdown_and_wait(node).await;
            }
        }
    }

    /// Bring `target` (and its pool, and every transitive dependency) up, in
    /// topological order so each dependency starts before its dependent. Clears
    /// `disabled` across the set. `OnDemand` (pool) members are only re-enabled,
    /// not force-spawned — the elastic policy re-grows them under load, which is
    /// the whole point of the pool.
    async fn activate(&self, target: &'static TaskNode, spawner: Spawner) {
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

/// Declare a supervised task graph and compute its topological order at compile
/// time (single source of nodes, deps, pool, and order). See the
/// `embassy-supervisor-macros` crate for the surface syntax.
#[cfg(feature = "macros")]
pub use embassy_supervisor_macros::supervisor_graph;

// ─── Tests (host-only) ─────────────────────────────────────────────────────
//
// Run on the host: `cargo test -p embassy-supervisor --target x86_64-unknown-linux-gnu`
// (the workspace `.cargo/config.toml` pins the embedded target, so `--target` is
// required to override it). These exercise the compile-time `topo_sort_const`
// over index adjacency tables — exactly what `supervisor_graph!` generates.
#[cfg(test)]
mod tests {
    use super::topo_sort_const;

    /// Position of index `x` within `order`.
    fn pos<const N: usize>(order: &[u8; N], x: u8) -> usize {
        order.iter().position(|&y| y == x).expect("index present")
    }

    #[test]
    fn linear_chain_orders_deps_before_dependents() {
        // A=0, B=1 dep A, C=2 dep B.
        const DEPS: [&[u8]; 3] = [&[], &[0], &[1]];
        const ORDER: [u8; 3] = topo_sort_const(&DEPS);
        assert_eq!(ORDER, [0, 1, 2]);
    }

    #[test]
    fn diamond_puts_root_first_and_join_last() {
        // A=0; B=1 dep A; C=2 dep A; D=3 dep B,C.
        const DEPS: [&[u8]; 4] = [&[], &[0], &[0], &[1, 2]];
        const ORDER: [u8; 4] = topo_sort_const(&DEPS);
        assert_eq!(ORDER[0], 0, "root first");
        assert_eq!(ORDER[3], 3, "join last");
        assert!(pos(&ORDER, 1) < pos(&ORDER, 3), "B before D");
        assert!(pos(&ORDER, 2) < pos(&ORDER, 3), "C before D");
    }

    #[test]
    fn independent_nodes_all_present() {
        const DEPS: [&[u8]; 2] = [&[], &[]];
        const ORDER: [u8; 2] = topo_sort_const(&DEPS);
        assert!(ORDER.contains(&0) && ORDER.contains(&1));
    }

    #[test]
    fn unsorted_input_is_sorted() {
        // Declared out of dependency order: node 0 depends on 1 and 2, node 2 on 1.
        const DEPS: [&[u8]; 3] = [&[1, 2], &[], &[1]];
        const ORDER: [u8; 3] = topo_sort_const(&DEPS);
        assert!(pos(&ORDER, 1) < pos(&ORDER, 2), "1 before 2");
        assert!(pos(&ORDER, 2) < pos(&ORDER, 0), "2 before 0");
    }

    #[test]
    fn evaluates_at_compile_time() {
        // The sort runs in a `const` context, proving it is const-evaluable.
        // (A cyclic table here would be a *compile* error, not a test failure.)
        const DEPS: [&[u8]; 3] = [&[], &[0], &[1]];
        const _: () = {
            let order = topo_sort_const(&DEPS);
            assert!(order[0] == 0 && order[1] == 1 && order[2] == 2);
        };
    }

    // Uncommenting this must fail to compile ("dependency cycle"):
    //   const CYCLE: [&[u8]; 2] = [&[1], &[0]];
    //   const _BAD: [u8; 2] = topo_sort_const(&CYCLE);
}
