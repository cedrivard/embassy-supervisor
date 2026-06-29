#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
//! # embassy-supervisor — a task-lifecycle supervisor for [embassy](https://embassy.dev)
//!
//! Application- and HAL-agnostic primitives for orchestrating a set of embassy
//! tasks: bringing them up in dependency order, tearing them down in reverse,
//! scaling an elastic worker pool with load, and starting/stopping/pausing/
//! resuming individual tasks at runtime while keeping the dependency graph
//! consistent.
//!
//! ## The model
//!
//!   * Each managed task is described by a [`TaskNode`] stored in a `static`.
//!   * Nodes declare their dependencies (`deps: &'static [&'static TaskNode]`)
//!     and a `spawn` fn the supervisor calls to launch them.
//!   * [`Supervisor`] topologically sorts the graph once at construction
//!     (Kahn's algorithm) and uses that ordering to bring tasks up in dependency
//!     order ([`Supervisor::start`]) and tear them down in reverse
//!     ([`Supervisor::teardown`]).
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
//! ## What the supervisor does *not* do
//!
//!   * It does not model any power-state transition (sleep/wake): it reacts to
//!     "teardown" and "bring-up" requests; the application drives them.
//!   * It does not allocate. Topo-sort scratch lives in a `heapless::Vec`.
//!   * It does not observe task internals. Tasks self-report their drop state via
//!     `ack_dropped()`; a task that fails to ack within a timeout panics the
//!     supervisor with the offending node's name.
//!
//! ## Cargo features
//!
//!   * `control` *(default)* — the runtime control plane: [`ControlOp`],
//!     [`request_control`], [`Supervisor::apply_control`].
//!   * `pool` *(default)* — elastic worker pools: [`ElasticPool`],
//!     [`Supervisor::with_pools`], [`Supervisor::run_pools`].
//!   * `defmt` — route the supervisor's logs through `defmt`; without it the log
//!     macros are no-ops.
//!
//! Build with `default-features = false` for a minimal core that only does
//! dependency-ordered bring-up/teardown (drops the control plane and pools,
//! trimming flash and a couple of statics).
//!
//! ## Example
//!
//! ```ignore
//! use embassy_executor::Spawner;
//! use embassy_supervisor::{task_graph, Mode, Supervisor, TaskNode, wait_control};
//!
//! // Each subsystem is a `TaskNode` wrapping a spawn fn; `app` depends on `net`.
//! static NET: TaskNode = TaskNode::new("net", Mode::Terminate, &[], |s| {
//!     s.spawn(net_task())?;
//!     Ok(())
//! });
//! static APP: TaskNode = TaskNode::new("app", Mode::Terminate, &[&NET], |s| {
//!     s.spawn(app_task())?;
//!     Ok(())
//! });
//!
//! task_graph! { &NET, &APP }   // emits `ALL_NODES` + `NODE_COUNT`
//!
//! #[embassy_executor::task]
//! async fn supervisor_task(spawner: Spawner) {
//!     let sup = Supervisor::new(&ALL_NODES).expect("no dependency cycle");
//!     sup.start(spawner).expect("initial spawn"); // brings up `net`, then `app`
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
//!
//! Docs:
//!   - `heapless::Vec` (no-alloc stack vector): <https://docs.rs/heapless>
//!   - `embassy_sync::signal::Signal`: <https://docs.embassy.dev/embassy-sync>
//!   - Kahn's algorithm (topological sort):
//!     <https://en.wikipedia.org/wiki/Topological_sorting#Kahn's_algorithm>

#[macro_use]
mod fmt;

use core::sync::atomic::Ordering;

use embassy_executor::{SpawnError, Spawner};
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
#[cfg(feature = "control")]
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use heapless::Vec;
use portable_atomic::AtomicBool;

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
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ControlOp {
    /// Bring the node up (start a stopped `Terminate` node, resume a `Pause` node).
    Activate,
    /// Take the node down (and its dependents, per the graph).
    Deactivate,
}

/// A runtime control request: drive `node` (and, per the dependency graph and
/// pool membership, the nodes it implies) in the `op` direction.
#[cfg(feature = "control")]
#[derive(Clone, Copy)]
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

// ─── Mode ────────────────────────────────────────────────────────────────

/// Lifecycle policy for a managed task: what the task does on shutdown and what
/// the supervisor does to bring it back.
#[derive(Clone, Copy, PartialEq, Eq)]
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
    /// The node manages its own lifecycle and must NOT be torn down by a
    /// dependency cascade. A dependency normally means "stop me when my dep stops",
    /// but a node can declare a dep purely for *start* ordering and then intend to
    /// outlive it — e.g. a node that needs a dependency up to initialize, then stops
    /// that dependency to reclaim its resources, must survive that teardown. While
    /// set, `deactivate` will not pull the node into a transitive-dependent teardown.
    /// Self-managed, so `reset()` leaves it (the node clears it itself when done).
    detached: AtomicBool,
}

impl TaskHandle {
    const fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            shutdown_wake: Signal::new(),
            dropped: AtomicBool::new(false),
            dropped_wake: Signal::new(),
            running: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            resume_wake: Signal::new(),
            disabled: AtomicBool::new(false),
            detached: AtomicBool::new(false),
        }
    }
}

// ─── TaskNode ────────────────────────────────────────────────────────────

/// A node in the supervisor's task graph.
///
/// Designed to live in `static` memory: every field is `Sync`, all constructors
/// are `const`. The application declares one per managed task and passes an
/// array of `&'static TaskNode` (typically via [`task_graph!`]) to
/// [`Supervisor::new`].
pub struct TaskNode {
    /// Human-readable name. Used in defmt logs and panic messages.
    pub name: &'static str,
    /// Lifecycle policy. See [`Mode`].
    pub mode: Mode,
    /// Nodes that must come up *before* this one (and tear down *after*).
    /// Pointer-equality is used during topological sort; references must point
    /// to other `static` `TaskNode`s in the same graph.
    pub deps: &'static [&'static TaskNode],
    /// App-provided spawn function (typically an inline closure at the node's
    /// declaration). Called once at boot from `Supervisor::start`, again from
    /// `respawn_terminate` for Terminate nodes, and at runtime from `start_node`
    /// for `OnDemand` nodes.
    pub spawn: fn(Spawner) -> Result<(), SpawnError>,
    handle: TaskHandle,
}

impl TaskNode {
    /// A single-instance node started at boot (`Terminate`/`Pause`) or on demand
    /// (`Mode::OnDemand`). Every node is single-instance; an elastic service is
    /// modelled as several `OnDemand` nodes of the same pooled task fn.
    pub const fn new(
        name: &'static str,
        mode: Mode,
        deps: &'static [&'static TaskNode],
        spawn: fn(Spawner) -> Result<(), SpawnError>,
    ) -> Self {
        Self { name, mode, deps, spawn, handle: TaskHandle::new() }
    }

    // ── Task-side API ────────────────────────────────────────────────────
    //
    // Called from inside the `#[embassy_executor::task] async fn` body.

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

    /// Mark this instance as having shut down. Every instance must call this
    /// exactly once on exit (Terminate/OnDemand mode) or on each pause (Pause
    /// mode) so the supervisor's `wait_dropped` can complete.
    pub fn ack_dropped(&self) {
        self.handle.dropped.store(true, Ordering::Release);
        self.handle.dropped_wake.signal(());
    }

    /// Pause-mode only: park until the supervisor signals resume.
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

    /// True while the node has been manually deactivated via the control
    /// interface and not yet re-activated. Read by a task-state view and by the
    /// automatic bring-up paths (which skip a disabled node).
    pub fn is_disabled(&self) -> bool {
        self.handle.disabled.load(Ordering::Acquire)
    }

    /// Mark/clear this node as **detached**: a node that manages its own lifecycle
    /// and must not be torn down by a dependency cascade (see the field doc). Set it
    /// before stopping a dependency the node has declared but intends to outlive.
    pub fn set_detached(&self, detached: bool) {
        self.handle.detached.store(detached, Ordering::Release);
    }

    /// True while this node is detached from dependency-cascade teardown.
    pub fn is_detached(&self) -> bool {
        self.handle.detached.load(Ordering::Acquire)
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

// ─── Task-graph declaration macro ─────────────────────────────────────────

/// Declares a supervisor task graph from one `cfg`-gated list, emitting both an
/// `ALL_NODES` array (what [`Supervisor::new`] sorts) and a `NODE_COUNT` const
/// (its length, which feeds the `const N` generic). Both expand from the same
/// tokens, so the count can't drift from the list. Invoke once in the
/// application, where the node `static`s are in scope:
///
/// ```ignore
/// supervisor::task_graph! {
///     &NET,
///     #[cfg(feature = "http")] &HTTP,
///     // …
/// }
/// // → pub const NODE_COUNT; pub static ALL_NODES: [&TaskNode; NODE_COUNT];
/// ```
///
/// Counting goes through cfg-gated `()` units (`[(); …].len()` is const; the
/// node refs aren't const-countable — a `const` can't refer to a `static`,
/// E0013). The cfg / no-cfg cases are split into separate `@munch` rules, with
/// the attribute matched as literal `#[cfg(...)]` tokens, to dodge the
/// `#[cfg] expr` ambiguity a single `$(#[$m:meta])*` rule hits (a cfg attribute
/// can legally prefix the `expr`, so the two can't be one fragment).
#[macro_export]
macro_rules! task_graph {
    // Done: emit the count + array from the two accumulated lists.
    (@munch units=[$($u:tt)*] nodes=[$($n:tt)*];) => {
        pub const NODE_COUNT: usize = [$($u)*].len();
        pub static ALL_NODES: [&$crate::TaskNode; NODE_COUNT] = [$($n)*];
    };
    // Next element carries a cfg attribute: push it to both lists.
    (@munch units=[$($u:tt)*] nodes=[$($n:tt)*]; #[cfg($c:meta)] $node:expr, $($rest:tt)*) => {
        $crate::task_graph!(@munch
            units=[$($u)* #[cfg($c)] (),]
            nodes=[$($n)* #[cfg($c)] $node,];
            $($rest)*
        );
    };
    // Next element is unconditional.
    (@munch units=[$($u:tt)*] nodes=[$($n:tt)*]; $node:expr, $($rest:tt)*) => {
        $crate::task_graph!(@munch
            units=[$($u)* (),]
            nodes=[$($n)* $node,];
            $($rest)*
        );
    };
    // Entry: seed the accumulators with the caller's list.
    ($($items:tt)*) => {
        $crate::task_graph!(@munch units=[] nodes=[]; $($items)*);
    };
}

// ─── Supervisor ──────────────────────────────────────────────────────────

/// Reasons [`Supervisor::new`] may fail. The node count `N` is derived from the
/// graph array, so the scratch buffers always fit — the only remaining failure
/// is a declared dependency `Cycle`.
#[derive(Debug)]
pub enum BuildError {
    /// The declared dependency graph contains a cycle.
    Cycle,
}

#[cfg(feature = "defmt")]
impl defmt::Format for BuildError {
    fn format(&self, f: defmt::Formatter) {
        match self {
            BuildError::Cycle => defmt::write!(f, "Cycle"),
        }
    }
}

/// Orchestrates a set of managed tasks across spawn / teardown / bring-up.
///
/// Owned by a single supervisor task. Concurrent access from other tasks goes
/// through each [`TaskNode`]'s own atomic state, not the `Supervisor` struct.
pub struct Supervisor<const N: usize> {
    nodes: &'static [&'static TaskNode],
    /// Topologically sorted indices into `nodes`. Dependencies appear before
    /// their dependents. The reverse iteration is the teardown order. `N` is the
    /// node count (from `ALL_NODES`), so this is sized exactly to the graph.
    order: Vec<u8, N>,
    /// Elastic pools, so the control interface can co-control a whole pool from
    /// any one member (`apply_control` expands the target through
    /// [`Pool::members`]) — the same registry `run_pools` drives. Defaults
    /// empty; set via [`Supervisor::with_pools`].
    #[cfg(feature = "pool")]
    pools: &'static [&'static dyn Pool],
}

impl<const N: usize> Supervisor<N> {
    /// Build a supervisor over `nodes`. `N` (the node count) is inferred from the
    /// array length, so the topo-sort scratch is sized exactly to the graph and
    /// the build can only fail on a dependency `Cycle`. Performs the topo sort
    /// eagerly so `start` / `teardown` / `respawn_terminate` are cheap.
    pub fn new(nodes: &'static [&'static TaskNode; N]) -> Result<Self, BuildError> {
        let order = topo_sort::<N>(nodes)?;
        let nodes: &'static [&'static TaskNode] = nodes;
        Ok(Self {
            nodes,
            order,
            #[cfg(feature = "pool")]
            pools: &[],
        })
    }

    /// Register the elastic pools so runtime control co-controls each pool as a
    /// unit (stopping/starting any member affects the whole pool). The same
    /// `&[&dyn Pool]` registry passed to `run_pools`. Builder-style so the call
    /// site reads `Supervisor::new(..)?.with_pools(POOLS)`.
    #[cfg(feature = "pool")]
    pub fn with_pools(mut self, pools: &'static [&'static dyn Pool]) -> Self {
        self.pools = pools;
        self
    }

    /// Spawn every boot node in dependency order. Called once at boot.
    /// `Mode::OnDemand` nodes are skipped — they're brought up at runtime by
    /// `start_node`. Pause-mode nodes often use a `|_| Ok(())` spawner and are
    /// spawned externally by `main()` (with hardware handles main owns); the
    /// no-op call here is harmless and still marks them `running`. Disabled nodes
    /// are skipped.
    pub fn start(&self, spawner: Spawner) -> Result<(), SpawnError> {
        for i in self.order.iter() {
            let node = self.nodes[*i as usize];
            if matches!(node.mode, Mode::OnDemand) || node.is_disabled() {
                continue;
            }
            info!("supervisor: spawning {} ({})", node.name, node.mode);
            (node.spawn)(spawner)?;
            node.set_running(true);
        }
        Ok(())
    }

    /// Start a single node at runtime — e.g. growing an elastic pool. Resets the
    /// handle, spawns one instance via the node's `spawn` fn (which must launch
    /// exactly one), and marks it `running`. Returns `SpawnError::Busy` if the
    /// underlying embassy task pool is exhausted (the ceiling), which the caller
    /// treats as "can't grow".
    pub fn start_node(
        &self,
        node: &'static TaskNode,
        spawner: Spawner,
    ) -> Result<(), SpawnError> {
        node.reset();
        (node.spawn)(spawner)?;
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
                node.name,
                SHUTDOWN_ACK_TIMEOUT_MS,
            );
        }
        node.set_running(false);
    }

    /// Stop a single running node at runtime — e.g. shrinking an elastic pool.
    /// Signals shutdown, waits for the ack, clears `running`. No-op if the node
    /// isn't running. Panics if it doesn't ack within the timeout.
    pub async fn stop_node(&self, node: &'static TaskNode) {
        if !node.is_running() {
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
            let node = self.nodes[*i as usize];
            if !node.is_running() {
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
    /// manual pause sticks; there is intentionally no dependency gate here.
    pub fn resume_pausable(&self) {
        for i in self.order.iter() {
            let node = self.nodes[*i as usize];
            if matches!(node.mode, Mode::Pause) && !node.is_disabled() {
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
    /// are skipped so a manual stop sticks across the bring-up. The reset happens
    /// before the spawn so newly-running tasks see a clean handle.
    pub fn respawn_terminate(&self, spawner: Spawner) -> Result<(), SpawnError> {
        for i in self.order.iter() {
            let node = self.nodes[*i as usize];
            if matches!(node.mode, Mode::Terminate) && !node.is_disabled() {
                node.reset();
                info!("supervisor: respawning {}", node.name);
                (node.spawn)(spawner)?;
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

#[cfg(feature = "control")]
impl<const N: usize> Supervisor<N> {
    /// Position of `node` in `self.nodes` (pointer identity — every node is a
    /// `&'static`). `None` only if the node isn't in this graph (impossible for
    /// targets sourced from `ALL_NODES`; treated as a no-op by callers).
    fn index_of(&self, node: &'static TaskNode) -> Option<usize> {
        self.nodes.iter().position(|n| core::ptr::eq(*n, node))
    }

    /// Seed a membership set with `target` plus — if `target` belongs to an
    /// elastic pool — every member of that pool, so control is applied to the
    /// whole pool atomically. Pool membership is read from the registry passed to
    /// `with_pools`; with no pools (the `pool` feature off, or none registered)
    /// this is just `{target}`.
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
            // A detached node declares its dep only for start ordering and intends
            // to outlive it, so it's never pulled into the cascade.
            if self.nodes[j].is_detached() {
                continue;
            }
            if self.nodes[j]
                .deps
                .iter()
                .any(|d| self.index_of(d).is_some_and(|di| set[di]))
            {
                set[j] = true;
            }
        }

        // Tear down in reverse topo order (dependents before their deps).
        for i in self.order.iter().rev() {
            let j = *i as usize;
            if !set[j] {
                continue;
            }
            let node = self.nodes[j];
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
        for i in self.order.iter().rev() {
            let j = *i as usize;
            if set[j] {
                for d in self.nodes[j].deps {
                    if let Some(di) = self.index_of(d) {
                        set[di] = true;
                    }
                }
            }
        }

        // Bring up in topo order (deps before dependents).
        for i in self.order.iter() {
            let j = *i as usize;
            if !set[j] {
                continue;
            }
            let node = self.nodes[j];
            node.set_disabled(false);
            if node.is_running() {
                continue;
            }
            match node.mode {
                Mode::Terminate => {
                    info!("supervisor: control-start {}", node.name);
                    // SpawnError::Busy (pool exhausted) → can't start, skip.
                    let _ = self.start_node(node, spawner);
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

// ─── Topological sort (Kahn's algorithm) ──────────────────────────────────
//
// Returns the node indices in dependency-first order: if A depends on B, then B
// appears before A in the result. The supervisor iterates this forward for
// `start` / `respawn_terminate` and in reverse for `teardown`.

fn topo_sort<const N: usize>(
    nodes: &'static [&'static TaskNode],
) -> Result<Vec<u8, N>, BuildError> {
    let n = nodes.len();

    // in_degree[i] = number of dependencies of nodes[i] not yet resolved.
    // Every scratch Vec has capacity N == nodes.len(), and each index is pushed
    // at most once, so no push can overflow; an impossible overflow would only
    // leave `order` short and surface harmlessly as `Cycle` via the check below.
    let mut in_degree: Vec<u8, N> = Vec::new();
    for node in nodes {
        let _ = in_degree.push(node.deps.len() as u8);
    }

    // Seed the queue with nodes that have no dependencies.
    let mut queue: Vec<u8, N> = Vec::new();
    for i in 0..n {
        if in_degree[i] == 0 {
            let _ = queue.push(i as u8);
        }
    }

    let mut order: Vec<u8, N> = Vec::new();
    let mut head = 0;
    while head < queue.len() {
        let i = queue[head] as usize;
        head += 1;
        let _ = order.push(i as u8);

        // Decrement the in-degree of every node that depends on nodes[i].
        // Pointer-equality works because `TaskNode`s live in `static` memory;
        // each reference to nodes[i] is the same address.
        for j in 0..n {
            if in_degree[j] == 0 {
                continue;
            }
            let depends_on_i = nodes[j]
                .deps
                .iter()
                .any(|d| core::ptr::eq(*d, nodes[i]));
            if depends_on_i {
                in_degree[j] -= 1;
                if in_degree[j] == 0 {
                    let _ = queue.push(j as u8);
                }
            }
        }
    }

    if order.len() != n {
        return Err(BuildError::Cycle);
    }
    Ok(order)
}

#[cfg(feature = "pool")]
mod pool;
#[cfg(feature = "pool")]
pub use pool::*;
