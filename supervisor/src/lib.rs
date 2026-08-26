//! Dependency-ordered task-lifecycle supervision for [embassy](https://embassy.dev) firmware.
//!
//! `embassy-supervisor` brings tasks up in dependency order, supervises their
//! lifecycle (`Terminate`, `Pause`, `OnDemand`), tears dependents down before
//! the things they depend on, and verifies declared dataflow against live
//! behaviour. The graph is declared through the [`supervisor_graph!`] macro
//! (re-exported from `embassy-supervisor-macros`) and checked at compile time.
//!
//! The crate is HAL-agnostic, `no_std`, and has no allocator or board-specific
//! dependencies.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

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
#[cfg(any(feature = "control", feature = "liveness-monitor"))]
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Timer, with_timeout};
#[cfg(feature = "liveness")]
use portable_atomic::AtomicBool;
#[cfg(feature = "liveness-monitor")]
use portable_atomic::AtomicU8;
use portable_atomic::AtomicU16;
#[cfg(any(feature = "trace", feature = "liveness", feature = "epochs"))]
use portable_atomic::AtomicU32;

#[cfg(feature = "pool")]
static SCALE_REQ: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Signal that an elastic pool should re-evaluate its scaling decision.
///
/// This is a no-op when the `pool` feature is disabled.
pub fn request_scale() {
    #[cfg(feature = "pool")]
    SCALE_REQ.signal(());
}

#[cfg(feature = "pool")]
/// Wait until something requests a pool scaling re-evaluation.
pub async fn wait_scale() {
    SCALE_REQ.wait().await;
}

static GATE_EVT: Signal<CriticalSectionRawMutex, ()> = Signal::new();

static STOP_EVT: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[doc(hidden)]
#[inline]
pub fn __sv_gate_event() {
    GATE_EVT.signal(());
}

#[cfg(feature = "control")]
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// A control request issued to the supervisor for a node.
pub enum ControlOp {
    /// Request that the node be activated.
    Activate,
    /// Request that the node be deactivated.
    Deactivate,
    /// Request that the node be restarted.
    #[cfg(feature = "restart")]
    Restart,
}

#[cfg(feature = "control")]
#[derive(Clone, Copy, Debug)]
/// A control request addressed to a specific node.
pub struct ControlCommand {
    /// The target node.
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

// ─── Declared dataflow coupling ───────────────────────────────────────────
//
// `deps:` says "spawn me after that". It is consumed once and says nothing
// about the relationship that holds for the rest of the program's life: the
// signals a task reads and writes. `reads:`/`writes:` declare *that* relation,
// which outlives the spawn the `deps:` edge described.

/// A signal a node declares it reads or writes.
///
/// Implemented for every `Sync` type by a blanket impl, so no consumer ever
/// writes one: the graph's `reads:`/`writes:` clauses coerce the named statics
/// to `&'static dyn CouplingPoint` for you. The trait carries **no methods** —
/// the supervisor neither knows nor cares what a signal is. Its only purpose is
/// to type-erase heterogeneous statics into one slice, so identity is the
/// static's address and nothing else.
#[cfg(feature = "coupling")]
pub trait CouplingPoint: Sync {}

#[cfg(feature = "coupling")]
impl<T: Sync + ?Sized> CouplingPoint for T {}

#[cfg(feature = "heap-state")]
pub use bytemuck::Zeroable;

#[cfg(feature = "coupling-observe")]
pub use embassy_supervisor_observe::Observable;

#[cfg(feature = "coupling-observe")]
#[derive(Clone, Copy)]
/// A callable that returns a signal's change count for observation.
pub struct Observer {
    count: fn() -> u32,
}

#[cfg(feature = "coupling-observe")]
impl Observer {
    /// Wrap a function that returns the signal's current change count.
    pub const fn new(count: fn() -> u32) -> Self {
        Self { count }
    }

    /// Return the current change count.
    pub fn count(&self) -> u32 {
        (self.count)()
    }
}

#[cfg(feature = "coupling")]
#[derive(Clone, Copy)]
/// A declared read/write coupling between a node and a signal.
pub struct Coupling {
    name: &'static str,
    point: &'static dyn CouplingPoint,
    #[cfg(feature = "coupling-observe")]
    observe: Option<Observer>,
    #[cfg(feature = "coupling-observe")]
    beat: bool,
}

#[cfg(feature = "coupling")]
impl Coupling {
    /// Create a coupling with the given path name and wiring point.
    pub const fn new(name: &'static str, point: &'static dyn CouplingPoint) -> Self {
        Self {
            name,
            point,
            #[cfg(feature = "coupling-observe")]
            observe: None,
            #[cfg(feature = "coupling-observe")]
            beat: false,
        }
    }

    #[cfg(feature = "coupling-observe")]
    /// Mark this coupling as observed with the given observer.
    pub const fn observed(mut self, observer: Observer) -> Self {
        self.observe = Some(observer);
        self
    }

    #[cfg(feature = "coupling-observe")]
    /// Mark this coupling as feeding a heartbeat.
    pub const fn beat(mut self) -> Self {
        self.beat = true;
        self
    }

    /// Return the path name of this coupling.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// This entry's accessor, if it carries the `observed` marker.
    #[cfg(feature = "coupling-observe")]
    pub const fn observer(&self) -> Option<Observer> {
        self.observe
    }

    /// Return whether this coupling feeds a heartbeat.
    #[cfg(feature = "coupling-observe")]
    pub const fn beats(&self) -> bool {
        self.beat
    }
}

#[cfg(feature = "coupling")]
impl core::fmt::Debug for Coupling {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name)
    }
}

/// Do two declarations refer to the same static? Compares **data pointers
/// only** (`ptr::addr_eq`): the same static reached through two different
/// generic instantiations can carry different vtables, and a plain
/// `ptr::eq` on trait objects would compare those too and miss the match.
#[cfg(feature = "coupling")]
fn same_signal(a: &Coupling, b: &Coupling) -> bool {
    core::ptr::addr_eq(
        a.point as *const dyn CouplingPoint,
        b.point as *const dyn CouplingPoint,
    )
}

/// Does `table` carry an entry whose declared path ends in the same segment as
/// `name`? Emitted by `supervisor_graph!` as a const assertion behind a marked
/// entry beside `discover`: the entry may only add a marker to a signal the
/// task fn already accesses, and this is what checks it before anything runs.
///
/// **By name, and only the last segment**, because a const context cannot
/// compare addresses: `ptr::addr_eq` is not `const`, and a pointer has no
/// integer value at compile time. The declaration and the call site usually
/// spell the same static differently (`crate::signals::EST` against an aliased
/// `s::EST`), so the tail is the most that can be matched. Two consequences,
/// both benign: a signal reached through a renaming re-export fails the check
/// though it is legitimate, and two distinct signals sharing a final segment
/// pass it. A marker that lands on the wrong static costs nothing silently —
/// no verb write matches it, so the node simply never beats and the liveness
/// monitor reports it stale.
#[doc(hidden)]
#[cfg(feature = "coupling")]
pub const fn __sv_tail_declared(table: &[Coupling], name: &str) -> bool {
    let mut i = 0;
    while i < table.len() {
        if tail_eq(table[i].name, name) {
            return true;
        }
        i += 1;
    }
    false
}

/// Byte offset just past the last `::` in `s`, or 0.
#[cfg(feature = "coupling")]
const fn tail_start(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = b.len();
    while i >= 2 {
        if b[i - 1] == b':' && b[i - 2] == b':' {
            return i;
        }
        i -= 1;
    }
    0
}

/// Do two paths end in the same segment, index and all (`ARR[1]`)?
#[cfg(feature = "coupling")]
const fn tail_eq(a: &str, b: &str) -> bool {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (ai, bi) = (tail_start(a), tail_start(b));
    if ab.len() - ai != bb.len() - bi {
        return false;
    }
    let mut k = 0;
    while ai + k < ab.len() {
        if ab[ai + k] != bb[bi + k] {
            return false;
        }
        k += 1;
    }
    true
}

// ─── Bound readiness edges (task → supervisor) ────────────────────────────
//
// A dedicated mailbox rather than a widening of CONTROL_REQ: a flapping link
// can produce readiness transitions far faster than an operator produces
// control commands, and the two must not be able to starve each other.

/// "Some node's readiness changed." A coalescing `Signal`, not a queue, and
/// deliberately so: the cascade handler re-reads live `is_ready` state rather
/// than trusting a message, so several transitions collapsing into one wake is
/// not a loss — it is the whole point. A flapping link cannot flood the
/// supervisor, and no readiness change can ever be dropped for lack of queue
/// space. Same single-consumer shape as `SCALE_REQ`: many tasks `signal()`,
/// only the supervisor `wait()`s.
#[cfg(feature = "bound-deps")]
static BIND_REQ: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Post a readiness transition. Sync and non-blocking — called from task
/// context inside `set_ready`/`clear_ready`, which must never park.
#[cfg(feature = "bound-deps")]
fn notify_bind() {
    BIND_REQ.signal(());
}

/// Await the next readiness transition of any node. The supervisor's driver
/// loop selects this alongside pool scaling and control.
#[cfg(feature = "bound-deps")]
pub async fn wait_bind() {
    BIND_REQ.wait().await
}

// ─── Health events (supervisor → app) ─────────────────────────────────────
//
// The reporting half of `liveness-monitor`. Deliberately a mailbox rather than
// a callback: the supervisor names what it saw, and the application decides —
// on its own task, at its own priority — what that means. See the module docs
// on why escalation is not built in.

/// What the monitor observed about a node.
///
/// `#[non_exhaustive]`: further observations may be added without a breaking
/// change, so match with a `_` arm.
#[cfg(feature = "liveness-monitor")]
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HealthKind {
    /// The node is still marked running but has not beaten within its
    /// `beat_timeout:` for `beat_window:` consecutive sweeps — alive but
    /// wedged, parked on an await that will never complete. Emitted **once**
    /// per stall; the next event for this node is a [`Recovered`](Self::Recovered).
    Stale {
        /// Ticks since the node's last beat when the sweep tripped.
        ticks: u32,
    },
    /// A node previously reported [`Stale`](Self::Stale) has beaten again.
    Recovered,
}

/// One observation from [`Supervisor::monitor`], delivered through
/// [`wait_health`].
#[cfg(feature = "liveness-monitor")]
#[derive(Clone, Copy, Debug)]
pub struct HealthEvent {
    /// The node the observation is about.
    pub node: &'static TaskNode,
    /// What was observed.
    pub kind: HealthKind,
}

#[cfg(all(feature = "liveness-monitor", feature = "defmt"))]
impl defmt::Format for HealthKind {
    fn format(&self, fmt: defmt::Formatter) {
        match self {
            HealthKind::Stale { ticks } => defmt::write!(fmt, "stale for {} ticks", ticks),
            HealthKind::Recovered => defmt::write!(fmt, "recovered"),
        }
    }
}

/// Supervisor → app health mailbox. Lossy on purpose (see
/// [`Supervisor::monitor`]): the monitor must never block on a slow or absent
/// consumer, and a still-stale node is re-reported by a later sweep anyway.
#[cfg(feature = "liveness-monitor")]
static HEALTH_EVT: Channel<CriticalSectionRawMutex, HealthEvent, 4> = Channel::new();

/// Await the next health observation from [`Supervisor::monitor`].
///
/// The application's escalation point. What to do with a `Stale` report is
/// domain-specific and therefore yours: log it, degrade to a safe mode, request
/// a `Deactivate`, `clear_ready()` the node so future bring-up defers — the
/// supervisor deliberately does none of these on its own.
///
/// ```ignore
/// loop {
///     let ev = embassy_supervisor::wait_health().await;
///     match ev.kind {
///         HealthKind::Stale { ticks } => warn!("{} wedged for {} ticks", ev.node.name, ticks),
///         HealthKind::Recovered => info!("{} is beating", ev.node.name),
///         _ => {}
///     }
/// }
/// ```
#[cfg(feature = "liveness-monitor")]
pub async fn wait_health() -> HealthEvent {
    HEALTH_EVT.receive().await
}

/// Non-blocking [`wait_health`], for a consumer that polls (a status endpoint
/// draining pending events, an existing loop that must not park here).
#[cfg(feature = "liveness-monitor")]
pub fn try_wait_health() -> Option<HealthEvent> {
    HEALTH_EVT.try_receive().ok()
}

/// Post one observation, dropping it if the app isn't keeping up. The monitor
/// runs on the supervisor task, which must not park behind a health consumer —
/// blocking here would stall pool scaling and control commands over a
/// diagnostic.
#[cfg(feature = "liveness-monitor")]
fn emit_health(ev: HealthEvent) {
    if HEALTH_EVT.try_send(ev).is_err() {
        warn!(
            "supervisor: health mailbox full, dropped an event for {}",
            ev.node.name()
        );
    }
}

/// Default per-node timeout for `wait_dropped` (`ack_timeout:` in the graph
/// overrides it per node). A task that doesn't ack within its window is a bug
/// (a body that never notices the stop) or a wedge; the shutdown paths
/// surface it as a [`NodeFault`] naming the node, and the application decides
/// the escalation. 2 s comfortably exceeds a typical task's poll period and
/// peripheral settle time.
const SHUTDOWN_ACK_TIMEOUT_MS: u64 = 2_000;

/// How much finer than its beat budget a `ready_on_write` node is probed while
/// it has yet to assert. Eight keeps the added readiness latency well inside
/// what a dependent's `slot_timeout` tolerates, without making bring-up busy.
#[cfg(all(
    feature = "liveness-monitor",
    feature = "coupling-observe",
    feature = "readiness"
))]
const READY_PROBE_DIVISOR: u64 = 8;

/// A node-scoped lifecycle failure: which node, and what went wrong.
///
/// Every way bring-up or teardown can fail is about one node, so one type says
/// so — the same `{ node, kind }` shape as [`HealthEvent`].
/// It is what [`Supervisor::start`], [`Supervisor::teardown`],
/// [`Supervisor::run`] and [`Supervisor::restart`] all return.
///
/// [`Display`](core::fmt::Display) is unconditional, so an application logging
/// through anything other than `defmt` can render one without matching on
/// [`FaultKind`].
#[derive(Clone, Copy, Debug)]
pub struct NodeFault {
    /// The node the failure is about.
    pub node: &'static TaskNode,
    /// What went wrong.
    pub kind: FaultKind,
}

/// What went wrong in a [`NodeFault`].
///
/// `#[non_exhaustive]`: new failure modes may be added without a breaking
/// change.
#[non_exhaustive]
#[derive(Clone, Copy, Debug)]
pub enum FaultKind {
    /// A `ready`-marked dep did not assert readiness within the dependent's
    /// `slot_timeout`. Either the dep is failing to reach its serving state, or
    /// the budget is too tight for this build.
    ReadyDepTimeout {
        /// The dep that never asserted.
        dep: &'static TaskNode,
    },
    /// A `resources:` slot was still empty at the deadline — nothing called
    /// `provide()` before the supervisor started, or a previous instance never
    /// restored it.
    ResourceMissing,
    /// The node names an `executor:` slot that was never filled with a spawner.
    ExecutorSlotEmpty,
    /// The spawn itself was rejected: the task's pool is exhausted, or (with
    /// `heap-state`) its `state:` allocation was refused. These are the only
    /// cases embassy's own `SpawnError` describes.
    Spawn(SpawnError),
    /// The node did not acknowledge a requested shutdown within its ack window.
    /// It is still marked running; the sane escalations are app-level.
    ShutdownTimeout,
}

impl core::fmt::Display for NodeFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.kind {
            FaultKind::ReadyDepTimeout { dep } => write!(
                f,
                "{}: ready-dep {} did not assert within {}ms",
                self.node.name(),
                dep.name(),
                self.node.slot_timeout().as_millis()
            ),
            FaultKind::ResourceMissing => {
                write!(
                    f,
                    "{}: a resource slot was never provided",
                    self.node.name()
                )
            }
            FaultKind::ExecutorSlotEmpty => {
                write!(
                    f,
                    "{}: its executor slot was never filled",
                    self.node.name()
                )
            }
            FaultKind::Spawn(_) => {
                write!(
                    f,
                    "{}: spawn failed, its task pool is exhausted",
                    self.node.name()
                )
            }
            FaultKind::ShutdownTimeout => {
                write!(f, "{}: missed its shutdown ack", self.node.name())
            }
        }
    }
}

#[cfg(feature = "defmt")]
impl defmt::Format for NodeFault {
    fn format(&self, fmt: defmt::Formatter) {
        match self.kind {
            FaultKind::ReadyDepTimeout { dep } => defmt::write!(
                fmt,
                "{}: ready-dep {} did not assert within {}ms",
                self.node.name(),
                dep.name(),
                self.node.slot_timeout().as_millis()
            ),
            FaultKind::ResourceMissing => {
                defmt::write!(
                    fmt,
                    "{}: a resource slot was never provided",
                    self.node.name()
                )
            }
            FaultKind::ExecutorSlotEmpty => {
                defmt::write!(
                    fmt,
                    "{}: its executor slot was never filled",
                    self.node.name()
                )
            }
            FaultKind::Spawn(_) => {
                defmt::write!(
                    fmt,
                    "{}: spawn failed, its task pool is exhausted",
                    self.node.name()
                )
            }
            FaultKind::ShutdownTimeout => {
                defmt::write!(fmt, "{}: missed its shutdown ack", self.node.name())
            }
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

/// The pause side of [`TaskNode::run_pausable`]'s result: a stop/pause request
/// won the race, the combinator acked and parked, and the supervisor has since
/// resumed the node. By the time a body sees `Err(Resumed)` the park is already
/// over — the next loop iteration is the fresh cycle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resumed;

#[cfg(feature = "defmt")]
impl defmt::Format for Resumed {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "resumed after pause");
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
    ///
    /// Shutdown is polled inline off `node` rather than stored as a
    /// `wait_shutdown()` future: the signal wait is stateless (register-on-poll),
    /// so embedding its state machine here would spend ~8 bytes of every
    /// caller's task storage carrying a second copy of the node reference.
    struct RunCancellable<'a, F> {
        #[pin]
        fut: Option<F>,
        node: &'a TaskNode,
        // True only for the `_acked` variant.
        ack: bool,
    }
}

impl<F: Future> Future for RunCancellable<'_, F> {
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
        // The flag fast path covers a request that predates this future (the
        // signal is edge-triggered); a fresh `wait()` per poll is sound because
        // the `Signal` latches its value and re-registers the waker each poll.
        if this.node.shutdown_requested()
            || core::pin::pin!(this.node.handle.shutdown_wake.wait())
                .poll(cx)
                .is_ready()
        {
            this.fut.set(None);
            if *this.ack {
                this.node.ack_dropped();
            }
            return Poll::Ready(Err(Aborted));
        }
        Poll::Pending
    }
}

pin_project_lite::pin_project! {
    /// The future behind [`TaskNode::run_pausable`]: [`RunCancellable`] with the
    /// `Pause` protocol's tail folded in. Racing, it behaves as the `_acked`
    /// variant; on abort it drops the worker in place, acks, and becomes the
    /// `wait_resume()` park — one state flag, and the worker's state machine is
    /// still held exactly once (same layout rationale as [`RunCancellable`]).
    struct RunPausable<'a, F> {
        #[pin]
        fut: Option<F>,
        node: &'a TaskNode,
        parked: bool,
    }
}

impl<F: Future> Future for RunPausable<'_, F> {
    type Output = Result<F::Output, Resumed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        if !*this.parked {
            // Same race, same ordering as `RunCancellable`: worker first, so a
            // completion in the same wake as a stop request still reports
            // completion; flag fast path, then the latched signal.
            if let Some(fut) = this.fut.as_mut().as_pin_mut()
                && let Poll::Ready(out) = fut.poll(cx)
            {
                return Poll::Ready(Ok(out));
            }
            if !this.node.shutdown_requested()
                && !core::pin::pin!(this.node.handle.shutdown_wake.wait())
                    .poll(cx)
                    .is_ready()
            {
                return Poll::Pending;
            }
            this.fut.set(None);
            this.node.ack_dropped();
            *this.parked = true;
            // Fall through to the resume poll in this same call: `resume_node`
            // is eligible the instant the ack lands, and a resume signaled
            // before this poll returns would otherwise latch with no waker
            // registered — a lost wakeup.
        }
        if core::pin::pin!(this.node.handle.resume_wake.wait())
            .poll(cx)
            .is_ready()
        {
            return Poll::Ready(Err(Resumed));
        }
        Poll::Pending
    }
}

/// How long the supervisor's bring-up waits for a node's `executor:`
/// [`SpawnerSlot`] to be filled before failing the spawn with
/// [`FaultKind::ExecutorSlotEmpty`]. A genuine cross-core rendezvous resolves in microseconds;
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

/// Renders [`as_str`](Self::as_str), so a `{}` in this crate's log macros reads
/// the same whichever backend is compiled in.
impl core::fmt::Display for Mode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── TaskHandle ──────────────────────────────────────────────────────────

/// Bit assignments of [`TaskHandle::flags`].
mod flag {
    /// Shutdown requested by the supervisor. Cleared by `reset()`.
    pub const SHUTDOWN: u16 = 1 << 0;
    /// The instance acked the shutdown (a flag, not a count, since every node
    /// is single-instance). Cleared by `reset()`.
    pub const DROPPED: u16 = 1 << 1;
    /// The supervisor has the node spawned and it hasn't exited. Always-on
    /// nodes are set by `start()`; `OnDemand` nodes by `start_node()` /
    /// `stop_node()`. `teardown()` only acts on running nodes, so a down
    /// `OnDemand` node doesn't stall it.
    pub const RUNNING: u16 = 1 << 2;
    /// Actively serving (`mark_busy()` / `mark_idle()`); read by the scaling
    /// policy.
    pub const BUSY: u16 = 1 << 3;
    /// The task body returned (`mark_exited()`). Cleared by `reset()`.
    /// Together with the lifecycle-spanning `SHUTDOWN` this distinguishes an
    /// autonomous completion (`COMPLETED && !SHUTDOWN`) from an acked stop.
    pub const COMPLETED: u16 = 1 << 4;
    /// Manually deactivated via the control interface, or declared `disabled`
    /// in the graph. **Lifecycle-spanning**: not cleared by `reset()`, so a
    /// manual stop sticks against the automatic bring-up paths until
    /// `Supervisor::activate`; living in a `static`, it also survives a
    /// RAM-retaining power-state transition.
    pub const DISABLED: u16 = 1 << 5;
    /// Self-managed: the supervisor never drives this node. Not cleared by
    /// `reset()`. Full rationale on `TaskNode::set_detached`.
    pub const DETACHED: u16 = 1 << 6;
    /// Task-asserted readiness ("initialized and serving") — distinct from
    /// `RUNNING` (spawned). Set by `set_ready()`, cleared by `clear_ready()`
    /// and by `reset()` so a respawned provider re-asserts.
    #[cfg(feature = "readiness")]
    pub const READY: u16 = 1 << 7;
    /// Stopped by a bound provider's `clear_ready`, eligible to come back
    /// when readiness returns. Deliberately NOT `DISABLED`: a manual stop must
    /// survive a readiness flap, and a bound stop must not survive the
    /// provider's recovery. Not cleared by `reset()` — it spans the stopped
    /// instance's whole absence.
    #[cfg(feature = "bound-deps")]
    pub const BOUND_STOPPED: u16 = 1 << 8;
}

/// Coordination state for one task. Embedded inside [`TaskNode`].
///
/// Every node is single-instance, so the state is one word of per-node flags
/// plus single-consumer signals — no counts, no fan-out. Written by one side
/// (task or supervisor) and read by the other: the supervisor requests exit
/// (`SHUTDOWN` + `shutdown_wake`), the task acks it (`DROPPED` +
/// `dropped_wake`), a parked Pause-mode task resumes on `resume_wake`, and
/// the scaling policy reads `RUNNING`/`BUSY`. See the private `flag` module
/// for each bit.
pub struct TaskHandle {
    /// The lifecycle flags, one bit each (see [`flag`]) — one atomic word
    /// where separate booleans would pad the handle. Writes pair `Release`
    /// with `Acquire` reads, per flag.
    flags: AtomicU16,
    /// Wake source for `wait_shutdown()`. Fired by `signal_shutdown()`.
    shutdown_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Wake source for `wait_dropped()`. Fired by `ack_dropped()`.
    dropped_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Wake source for `wait_resume()` on Pause-mode tasks. Fired by
    /// `signal_resume()`.
    resume_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Wake source for `wait_ready()`. Latching; the supervisor's bring-up is
    /// the only pre-fill waiter (single-waiter Signal semantics).
    #[cfg(feature = "readiness")]
    ready_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Instant ticks (truncated) of the last `beat()`; also stamped by
    /// `set_running(true)` so a freshly spawned node is never instantly stale.
    #[cfg(feature = "liveness")]
    last_beat: AtomicU32,
    /// Activation generation, bumped on every false→true `running` transition.
    /// Starts at 0 ("never activated"); the first spawn makes it 1. Wrapping —
    /// dependents compare for *inequality*, never ordering.
    #[cfg(feature = "epochs")]
    epoch: AtomicU32,
    /// Wake source for `wait_epoch_change()`. Single-waiter Signal, like
    /// `ready_wake`; the multi-consumer path is polling `epoch()`.
    #[cfg(feature = "epochs")]
    epoch_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Consecutive monitor sweeps that found this node stale. Reset to 0 by any
    /// sweep that finds it beating. Saturates at `beat_window`: once the event
    /// has been emitted the count only needs to hold the threshold, so a node
    /// stale for hours cannot wrap back around and re-report.
    #[cfg(feature = "liveness-monitor")]
    stale_strikes: AtomicU8,
    /// Wrapping sum of this node's beat-feeding `observed` write counters as
    /// of the last sweep. One word per node rather than one per entry, which
    /// is why a `beat` entry's token must be a counter — a documented
    /// requirement on [`Observer`], since value-tokens could cancel in the sum.
    #[cfg(all(feature = "coupling-observe", feature = "liveness"))]
    write_mark: AtomicU32,
    /// The node beat since anyone last looked;
    /// [`TaskNode::ticks_since_beat`] converts it into a timestamp with the
    /// `now` it reads anyway. One relaxed store per beat instead of a timer
    /// read — the write-rate decimation, with the clock cost on the checker,
    /// and the reason [`TaskNode::beat`] is cheap enough to call per message.
    #[cfg(feature = "liveness")]
    pending_beat: AtomicBool,
    /// The node's self-description (`report_status`), shown when asked and
    /// never acted on. A mutexed cell because a `&'static str` is two words —
    /// too wide to swap atomically.
    #[cfg(feature = "node-status")]
    status: BlockingMutex<CriticalSectionRawMutex, Cell<Option<&'static str>>>,
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
            flags: AtomicU16::new(if disabled_at_boot { flag::DISABLED } else { 0 }),
            shutdown_wake: Signal::new(),
            dropped_wake: Signal::new(),
            resume_wake: Signal::new(),
            #[cfg(feature = "readiness")]
            ready_wake: Signal::new(),
            #[cfg(feature = "liveness")]
            last_beat: AtomicU32::new(0),
            #[cfg(feature = "epochs")]
            epoch: AtomicU32::new(0),
            #[cfg(feature = "epochs")]
            epoch_wake: Signal::new(),
            #[cfg(feature = "liveness-monitor")]
            stale_strikes: AtomicU8::new(0),
            #[cfg(all(feature = "coupling-observe", feature = "liveness"))]
            write_mark: AtomicU32::new(0),
            #[cfg(feature = "liveness")]
            pending_beat: AtomicBool::new(false),
            #[cfg(feature = "node-status")]
            status: BlockingMutex::new(Cell::new(None)),
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

    fn flag(&self, bit: u16) -> bool {
        self.flags.load(Ordering::Acquire) & bit != 0
    }

    fn flag_set(&self, bit: u16) {
        self.flags.fetch_or(bit, Ordering::Release);
    }

    /// Clear `bits` — one bit or a whole mask, as `reset()` uses it.
    fn flag_clear(&self, bits: u16) {
        self.flags.fetch_and(!bits, Ordering::Release);
    }

    fn flag_put(&self, bit: u16, on: bool) {
        if on {
            self.flag_set(bit);
        } else {
            self.flag_clear(bit);
        }
    }

    /// Set or clear `bit` and report whether it was set before — the
    /// transition test `mark_busy` / `mark_idle` signal scaling on.
    fn flag_swap(&self, bit: u16, on: bool) -> bool {
        let prior = if on {
            self.flags.fetch_or(bit, Ordering::Release)
        } else {
            self.flags.fetch_and(!bit, Ordering::Release)
        };
        prior & bit != 0
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
/// sup.start(&spawner).await?;   // nodes declared `executor: HIGH` spawn on that tier
/// ```
///
/// The supervisor's bring-up (`start` / `start_node` / `respawn_terminate`) awaits
/// [`ready`](Self::ready) for a node's slot before spawning it, so a tier filled
/// late — or from another core — is handled without a race; a slot still empty after
/// the supervisor's bounded wait fails the spawn with [`FaultKind::ExecutorSlotEmpty`]
/// rather than silently dropping the task. Spawned futures must be `Send` (a non-`Send`
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
        __sv_gate_event();
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
    /// Empty the slot, dropping any held value — how a stopping provider's
    /// `provides:` list is cleared, so a value that dies with its producer
    /// reads empty until the next activation re-provides. Default no-op, for a
    /// gate view with nothing to clear.
    fn clear(&self) {}
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
/// 2. The bring-up awaits the slot being filled, then the generated task
///    shell [`take`](Self::take)s it — inside the spawned task, so a value
///    never crosses the spawn call. A slot still empty at the gate deadline
///    fails the spawn with [`FaultKind::ResourceMissing`] — a fail-closed
///    error out of [`Supervisor::start`], not a panic inside the task
///    (compare `static_cell::StaticCell`, which panics on misuse).
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
    /// with `FaultKind::ExecutorSlotEmpty`. Filling an occupied slot replaces (drops) the
    /// old value — don't: one resource, one slot, moved exactly once.
    pub fn provide(&self, value: T) {
        self.slot.lock(|c| c.set(Some(value)));
        self.filled.signal(());
        __sv_gate_event();
    }

    /// Take the resource out, leaving the slot empty. Called by the generated
    /// task shell at its first poll; `None` means "not provided yet" or
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

    /// Empty the slot, dropping any held value, and reset the latched filled
    /// signal. The provider side of the freshness convention: a task that
    /// rebuilds this slot's value each activation clears it on the way down —
    /// the `provides:` clause does it from the shutdown ack — so a consumer's
    /// gate wait holds for the next activation's value instead of taking this
    /// one's leftover.
    pub fn clear(&self) {
        let stale = self.slot.lock(Cell::take);
        drop(stale);
        self.filled.reset();
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

    fn clear(&self) {
        ResourceSlot::clear(self);
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
///
/// Split in two on purpose. The handle's atomics force this static into RAM —
/// and would force everything beside them into RAM too — so the node holds
/// only its live state plus one reference to its [`NodeCfg`], the immutable
/// half (name, mode, spawn fn, gates, budgets, coupling tables), which has no
/// interior mutability and therefore stays in flash. Same lesson as the
/// graph's [`Topology`], one level down: keep the constant data out of reach
/// of the atomics.
pub struct TaskNode {
    /// The immutable half — flash-resident, emitted as its own `static` by
    /// [`supervisor_graph!`] beside the node.
    cfg: &'static NodeCfg,
    handle: TaskHandle,
}

/// The immutable half of a [`TaskNode`]: everything the graph *declared* about
/// the node, none of what happens to it at runtime. No interior mutability, so
/// the `static` carrying it lives in flash (`.rodata`); the RAM-resident node
/// points at it. Built `const` with [`new`](Self::new) plus the chainable
/// `with_*` methods, exactly as [`supervisor_graph!`] emits it.
pub struct NodeCfg {
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
    /// `Some`, the supervisor holds the spawn until the slot is filled
    /// (bounded by [`slot_timeout`](TaskNode::slot_timeout)), so the
    /// generated glue's own non-blocking `SpawnerSlot::get` is already filled. Set
    /// by the macro via [`with_executor`](Self::with_executor); `const`, zero-cost.
    spawn_slot: Option<&'static SpawnerSlot>,
    /// The [`ResourceSlot`]s this node's spawn takes from (`resources:` in the
    /// graph), type-erased to their [`ResourceGate`] readiness view. The
    /// supervisor holds the spawn until every gate is filled (bounded by
    /// [`slot_timeout`](TaskNode::slot_timeout)), so (a) a `main` that
    /// provides late is tolerated and (b) a respawn cannot race the previous
    /// instance's shell restoring the value (the restore happens after the
    /// worker's shutdown ack). Empty for nodes without `resources:`. Set by the
    /// macro via [`with_resources`](Self::with_resources); `const`, zero-cost.
    resource_gates: &'static [&'static dyn ResourceGate],
    /// The slots this node's task fills at runtime (`provides:` in the graph),
    /// cleared when the node acknowledges a stop so a consumer's gate wait
    /// sees the value's absence rather than a previous activation's leftover.
    /// A `Pause` ack is exempt: the parked task still backs what it published.
    provides: &'static [&'static dyn ResourceGate],
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
    /// How long a stop waits for this node's shutdown ack before faulting it
    /// (`ack_timeout:` in the graph). Defaults to [`SHUTDOWN_ACK_TIMEOUT_MS`]
    /// (2 s); raise it for a node whose cleanup legitimately takes longer —
    /// a flash sync, a peripheral settle. Set by the macro via
    /// [`with_ack_timeout`](Self::with_ack_timeout).
    ack_timeout: embassy_time::Duration,
    /// How long this node may go without a `beat()` before the monitor calls it
    /// stale (`beat_timeout:` in the graph). `None` — the default — opts the
    /// node OUT of policing entirely, which is right for every node whose body
    /// does not beat: it would otherwise read permanently stale.
    #[cfg(feature = "liveness-monitor")]
    beat_timeout: Option<embassy_time::Duration>,
    /// How many *consecutive* stale sweeps are needed before the monitor emits
    /// (`beat_window:` in the graph). 1 (the default) reports the first miss;
    /// raise it to tolerate a node whose beat interval is legitimately jittery.
    #[cfg(feature = "liveness-monitor")]
    beat_window: u8,
    /// `ready_on_write` in the graph: the sweep calls [`TaskNode::set_ready`]
    /// the first time an `observed` write advances, instead of the task
    /// asserting readiness itself.
    #[cfg(all(feature = "coupling-observe", feature = "readiness"))]
    ready_on_write: bool,
    /// The signals this node consumes, as one table per source — the
    /// `reads:` list, a `discover` node's derived table, each adopted
    /// `dataflow:` fn's table. Read by the signal-indexed queries and the
    /// diagram tool; never by the spawn machinery.
    #[cfg(feature = "coupling")]
    reads: &'static [&'static [Coupling]],
    /// The write-side tables. An `observed` entry here is additionally polled
    /// by [`Supervisor::monitor`], which turns an advance into a beat.
    #[cfg(feature = "coupling")]
    writes: &'static [&'static [Coupling]],
    /// The `bound`-marked subset of `deps:` — providers whose readiness
    /// *controls* this node rather than merely gating its first spawn.
    #[cfg(feature = "bound-deps")]
    bound_deps: &'static [&'static TaskNode],
    /// The graph this node belongs to — its view of its own peers, and what a
    /// data-driven dependency resolves a producer through. Set by
    /// [`supervisor_graph!`]; the graph names the nodes and each node names the
    /// graph, which is a cycle only in the address sense and so is a perfectly
    /// ordinary pair of statics.
    ///
    /// [`NO_GRAPH`](graph_ref::NO_GRAPH) for a hand-built node that belongs to
    /// no graph, which simply has no peers to answer about.
    #[cfg(feature = "data-deps")]
    graph: &'static GraphRef,
}

impl NodeCfg {
    /// The declared side of a single-instance node started at boot
    /// (`Terminate`/`Pause`) or on demand (`Mode::OnDemand`). Every node is
    /// single-instance; an elastic service is modelled as several `OnDemand`
    /// nodes of the same pooled task fn.
    ///
    /// A node carries only its own identity and behaviour; the graph's
    /// dependency edges live in the compile-time index table that
    /// [`supervisor_graph!`] emits and [`Supervisor::new`] consumes.
    /// `spawn` is `None` for a parked node the application spawns itself.
    pub const fn new(
        name: &'static str,
        mode: Mode,
        spawn: Option<fn(Spawner) -> Result<(), SpawnError>>,
    ) -> Self {
        Self {
            name,
            mode,
            spawn,
            spawn_slot: None,
            resource_gates: &[],
            provides: &[],
            #[cfg(feature = "readiness")]
            ready_deps: &[],
            slot_timeout: SLOT_READY_TIMEOUT,
            ack_timeout: embassy_time::Duration::from_millis(SHUTDOWN_ACK_TIMEOUT_MS),
            #[cfg(feature = "liveness-monitor")]
            beat_timeout: None,
            #[cfg(feature = "liveness-monitor")]
            beat_window: 1,
            #[cfg(all(feature = "coupling-observe", feature = "readiness"))]
            ready_on_write: false,
            #[cfg(feature = "coupling")]
            reads: &[],
            #[cfg(feature = "coupling")]
            writes: &[],
            #[cfg(feature = "bound-deps")]
            bound_deps: &[],
            #[cfg(feature = "data-deps")]
            graph: &graph_ref::NO_GRAPH,
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

    /// Declare the slots this node's task fills at runtime (the `provides:`
    /// graph clause); a stop ack clears them. `const` and chainable in a
    /// `static` initializer; emitted by [`supervisor_graph!`].
    pub const fn with_provides(mut self, slots: &'static [&'static dyn ResourceGate]) -> Self {
        self.provides = slots;
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
    /// `NodeFault`, just later). `const` and chainable in a `static`
    /// initializer; emitted by [`supervisor_graph!`].
    pub const fn with_slot_timeout(mut self, timeout: embassy_time::Duration) -> Self {
        self.slot_timeout = timeout;
        self
    }

    /// Override how long a stop waits for this node's shutdown ack before
    /// faulting it with [`FaultKind::ShutdownTimeout`] (the
    /// `ack_timeout: <millis>` graph clause, default 2 s). Raise it for a node
    /// whose cleanup legitimately outlasts the default — a flash sync, a
    /// peripheral settle; the missed-ack failure mode stays a loud
    /// [`NodeFault`], just later. `const` and chainable in a `static`
    /// initializer; emitted by [`supervisor_graph!`].
    pub const fn with_ack_timeout(mut self, timeout: embassy_time::Duration) -> Self {
        self.ack_timeout = timeout;
        self
    }

    /// Opt this node into liveness policing (the `beat_timeout: <millis>` graph
    /// clause): [`Supervisor::monitor`] reports it once it has been running
    /// without a [`beat`](TaskNode::beat) for longer than `timeout`.
    ///
    /// Only declare this on a node whose body actually beats — an un-beating
    /// node reads permanently stale. `const` and chainable in a `static`
    /// initializer; emitted by [`supervisor_graph!`].
    #[cfg(feature = "liveness-monitor")]
    pub const fn with_beat_timeout(mut self, timeout: embassy_time::Duration) -> Self {
        self.beat_timeout = Some(timeout);
        self
    }

    /// How many consecutive stale sweeps the monitor requires before it reports
    /// this node (the `beat_window: <n>` graph clause, default 1). Raise it for
    /// a node whose beat interval is legitimately jittery — the effective
    /// grace period becomes roughly `beat_timeout` + `n` sweep periods.
    ///
    /// `0` is treated as `1`. `const` and chainable in a `static` initializer;
    /// emitted by [`supervisor_graph!`].
    #[cfg(feature = "liveness-monitor")]
    pub const fn with_beat_window(mut self, sweeps: u8) -> Self {
        self.beat_window = if sweeps == 0 { 1 } else { sweeps };
        self
    }

    /// Let an observed write assert readiness (the `ready_on_write` graph
    /// clause).
    ///
    /// The sweep calls [`set_ready`](TaskNode::set_ready) the first time one of this
    /// node's `observed` writes advances, so "ready" means "actually producing"
    /// rather than "reached the line where it says so". Requires
    /// `beat_timeout:`, which is what puts the node in the sweep at all.
    ///
    /// Monotone by design: it never withdraws readiness. A node that goes quiet
    /// is reported through [`wait_health`], and what to do about that stays the
    /// application's decision.
    #[cfg(all(feature = "coupling-observe", feature = "readiness"))]
    pub const fn with_ready_on_write(mut self) -> Self {
        self.ready_on_write = true;
        self
    }

    /// Declare the signals this node consumes (the `reads:` graph clause).
    /// Purely descriptive: the supervisor never gates on it, and reads carry
    /// neither heartbeat nor readiness. What it buys is a graph that says what
    /// the node consumes — to [`Graph::readers_of`], to the diagram tool.
    /// `const` and chainable
    /// in a `static` initializer; emitted by [`supervisor_graph!`].
    #[cfg(feature = "coupling")]
    pub const fn with_reads(mut self, reads: &'static [&'static [Coupling]]) -> Self {
        self.reads = reads;
        self
    }

    /// Declare the signals this node produces (the `writes:` graph clause).
    /// See [`with_reads`](Self::with_reads).
    #[cfg(feature = "coupling")]
    pub const fn with_writes(mut self, writes: &'static [&'static [Coupling]]) -> Self {
        self.writes = writes;
        self
    }

    /// Declare the `bound`-marked subset of `deps:` — providers whose
    /// readiness controls this node. `const` and chainable in a `static`
    /// initializer; emitted by [`supervisor_graph!`].
    #[cfg(feature = "bound-deps")]
    pub const fn with_bound_deps(mut self, deps: &'static [&'static TaskNode]) -> Self {
        self.bound_deps = deps;
        self
    }

    /// Point the node at its own graph. `const` and chainable in a `static`
    /// initializer; emitted by [`supervisor_graph!`].
    #[cfg(feature = "data-deps")]
    pub const fn with_graph(mut self, graph: &'static GraphRef) -> Self {
        self.graph = graph;
        self
    }
}

impl TaskNode {
    /// A node over its flash-resident [`NodeCfg`]. `disabled_at_boot` seeds
    /// the node's disabled flag so a control-started node (e.g. an OTA task)
    /// can be declared down and started later via a control op. `const`;
    /// [`supervisor_graph!`] emits the config `static` and this call together.
    pub const fn new(cfg: &'static NodeCfg, disabled_at_boot: bool) -> Self {
        Self {
            cfg,
            handle: TaskHandle::new(disabled_at_boot),
        }
    }

    /// Human-readable name. Used in defmt logs and panic messages.
    pub const fn name(&self) -> &'static str {
        self.cfg.name
    }

    /// Lifecycle policy. See [`Mode`].
    pub const fn mode(&self) -> Mode {
        self.cfg.mode
    }

    /// Does this node let an observed write assert its readiness?
    #[cfg(all(feature = "coupling-observe", feature = "readiness"))]
    pub const fn ready_on_write(&self) -> bool {
        self.cfg.ready_on_write
    }

    // ── Declaration getters ──────────────────────────────────────────────
    //
    // The `with_*` builders are write-only from the application's side; these
    // read back what the graph declared, so a status endpoint or a diagnostic
    // can report the configuration alongside the live state.

    /// This node's pre-spawn gate-wait bound (see
    /// [`with_slot_timeout`](NodeCfg::with_slot_timeout)). The whole-graph
    /// waves budget all of a node's gates together, from when its in-pass deps
    /// resolve; the single-node path ([`start_node`](Supervisor::start_node))
    /// gives each gate — executor slot, each `resources:` slot, each `ready`
    /// dep — the full budget.
    pub const fn slot_timeout(&self) -> embassy_time::Duration {
        self.cfg.slot_timeout
    }

    /// How long a stop waits for this node's shutdown ack before faulting it
    /// (see [`with_ack_timeout`](NodeCfg::with_ack_timeout)). Both stop paths
    /// honor it: the single-node wait, and the whole-graph wave, where each
    /// node's window runs from the moment *it* is signalled.
    pub const fn ack_timeout(&self) -> embassy_time::Duration {
        self.cfg.ack_timeout
    }

    /// The deps whose readiness bring-up awaits before spawning this node (the
    /// `ready`-marked subset of `deps:`). Empty when none are marked.
    #[cfg(feature = "readiness")]
    pub const fn ready_deps(&self) -> &'static [&'static TaskNode] {
        self.cfg.ready_deps
    }

    /// This node's liveness budget, or `None` when it is not policed (see
    /// [`with_beat_timeout`](NodeCfg::with_beat_timeout)).
    #[cfg(feature = "liveness-monitor")]
    pub const fn beat_timeout(&self) -> Option<embassy_time::Duration> {
        self.cfg.beat_timeout
    }

    /// Consecutive stale sweeps required before the monitor reports this node
    /// (see [`with_beat_window`](NodeCfg::with_beat_window)).
    #[cfg(feature = "liveness-monitor")]
    pub const fn beat_window(&self) -> u8 {
        self.cfg.beat_window
    }

    /// The signals this node declares it consumes (`reads:`).
    #[cfg(feature = "coupling")]
    pub const fn reads(&self) -> &'static [&'static [Coupling]] {
        self.cfg.reads
    }

    /// The signals this node declares it produces (`writes:`).
    #[cfg(feature = "coupling")]
    pub const fn writes(&self) -> &'static [&'static [Coupling]] {
        self.cfg.writes
    }

    /// Every coupling entry in one direction: tables in bound order, entries
    /// in table order.
    #[cfg(feature = "coupling")]
    fn entries(&self, is_write: bool) -> impl Iterator<Item = &'static Coupling> {
        let tables = if is_write {
            self.cfg.writes
        } else {
            self.cfg.reads
        };
        tables.iter().flat_map(|t| t.iter())
    }

    /// Is `signal` among this node's entries in the given direction?
    #[cfg(feature = "coupling")]
    fn has_entry(&self, signal: &Coupling, is_write: bool) -> bool {
        self.entries(is_write).any(|e| same_signal(e, signal))
    }

    /// The `bound`-marked subset of `deps:` (see
    /// [`with_bound_deps`](NodeCfg::with_bound_deps)).
    #[cfg(feature = "bound-deps")]
    pub const fn bound_deps(&self) -> &'static [&'static TaskNode] {
        self.cfg.bound_deps
    }

    /// The node slots of the graph this node belongs to, `#[cfg]`-ed-out slots
    /// included as `None` — the same table [`Graph::nodes`] exposes, reached
    /// from a node rather than from the graph static. Empty for a node no
    /// graph declared.
    #[cfg(feature = "data-deps")]
    pub const fn graph(&self) -> &'static [Option<&'static TaskNode>] {
        self.cfg.graph.nodes()
    }

    /// True while this node is down because a bound provider withdrew
    /// readiness — as opposed to `is_disabled`, which means somebody stopped it
    /// on purpose. The distinction matters: a bound stop must lift by itself
    /// when the provider recovers, and a manual stop must not.
    #[cfg(feature = "bound-deps")]
    pub fn is_bound_stopped(&self) -> bool {
        self.handle.flag(flag::BOUND_STOPPED)
    }

    // ── Task-side API ────────────────────────────────────────────────────
    //
    // Called from inside the `#[embassy_executor::task] async fn` body. The
    // whole task-side protocol is four rules (the README's "Writing supervised

    /// Return `true` if the supervisor has asked this node to shut down.
    pub fn shutdown_requested(&self) -> bool {
        self.handle.flag(flag::SHUTDOWN)
    }

    /// Wait until the supervisor asks this node to shut down.
    pub async fn wait_shutdown(&self) {
        if self.handle.flag(flag::SHUTDOWN) {
            return;
        }
        self.handle.shutdown_wake.wait().await;
    }

    /// Acknowledge that this node's instance has dropped and notify waiters.
    pub fn ack_dropped(&self) {
        if !matches!(self.cfg.mode, Mode::Pause) {
            for gate in self.cfg.provides {
                gate.clear();
            }
        }
        self.handle.flag_clear(flag::RUNNING);
        self.handle.flag_set(flag::DROPPED);
        self.handle.dropped_wake.signal(());
        STOP_EVT.signal(());
        #[cfg(feature = "bound-deps")]
        notify_bind();
    }

    /// Mark the node as completed and then acknowledge its drop.
    pub fn mark_exited(&self) {
        self.handle.flag_set(flag::COMPLETED);
        self.ack_dropped();
    }

    #[doc(hidden)]
    pub fn mark_lost_resource(&self) {
        warn!(
            "supervisor: {} lost a resource between spawn and first poll",
            self.name()
        );
        self.mark_exited();
    }

    /// Return `true` if the node has been marked as exited.
    pub fn has_exited(&self) -> bool {
        self.handle.flag(flag::COMPLETED)
    }

    #[cfg(feature = "readiness")]
    /// Assert this node's readiness and wake dependents waiting on it.
    pub fn set_ready(&self) {
        self.handle.flag_set(flag::READY);
        self.handle.ready_wake.signal(());
        __sv_gate_event();
        #[cfg(feature = "data-deps")]
        crate::data_deps::notify_serving();
        #[cfg(feature = "bound-deps")]
        notify_bind();
    }

    #[cfg(feature = "readiness")]
    /// Clear this node's readiness.
    pub fn clear_ready(&self) {
        self.handle.flag_clear(flag::READY);
        #[cfg(feature = "bound-deps")]
        notify_bind();
    }

    #[cfg(feature = "readiness")]
    /// Return whether this node is currently ready.
    pub fn is_ready(&self) -> bool {
        self.handle.flag(flag::READY)
    }

    #[cfg(feature = "readiness")]
    /// Wait until this node becomes ready.
    pub async fn wait_ready(&self) {
        loop {
            if self.is_ready() {
                return;
            }
            self.handle.ready_wake.wait().await;
        }
    }

    #[cfg(all(feature = "pool", feature = "readiness"))]
    pub(crate) fn ready_deps_ok(&self) -> bool {
        self.cfg.ready_deps.iter().all(|d| d.is_ready())
    }
    #[cfg(all(feature = "pool", not(feature = "readiness")))]
    pub(crate) fn ready_deps_ok(&self) -> bool {
        true
    }

    #[cfg(feature = "liveness")]
    #[inline]
    /// Record a liveness beat for this node.
    pub fn beat(&self) {
        self.handle.pending_beat.store(true, Ordering::Relaxed);
    }

    #[cfg(all(feature = "coupling-observe", feature = "liveness"))]
    /// Return whether any observed beat coupling has changed since last call.
    pub fn poll_observed_writes(&self) -> bool {
        let mut mark = 0u32;
        let mut any = false;
        for w in self.entries(true) {
            if !w.beats() {
                continue;
            }
            if let Some(o) = w.observer() {
                mark = mark.wrapping_add(o.count());
                any = true;
            }
        }
        any && self.handle.write_mark.swap(mark, Ordering::AcqRel) != mark
    }

    #[cfg(all(feature = "coupling-observe", feature = "liveness"))]
    fn seed_write_mark(&self) {
        let mut mark = 0u32;
        for w in self.entries(true) {
            if w.beats()
                && let Some(o) = w.observer()
            {
                mark = mark.wrapping_add(o.count());
            }
        }
        self.handle.write_mark.store(mark, Ordering::Release);
    }

    #[cfg(feature = "node-status")]
    /// Report a status string for this node.
    pub fn report_status(&self, status: &'static str) {
        let prev = self.handle.status.lock(|s| s.replace(Some(status)));
        // `&'static str`s for a status are typically literals, so pointer
        // inequality is "changed" for logging purposes; a same-text status
        // reached through two literals logs once more, harmlessly.
        if prev.is_none_or(|p| !core::ptr::eq(p.as_ptr(), status.as_ptr())) {
            info!("supervisor: {}: {}", self.cfg.name, status);
        }
    }

    /// The node's current self-description, if it reported one this activation.
    #[cfg(feature = "node-status")]
    pub fn status(&self) -> Option<&'static str> {
        self.handle.status.lock(|s| s.get())
    }

    /// Ticks since the last [`beat`](Self::beat) — where, with
    /// `dataflow`, a write through the node's verbs since the previous
    /// call counts as one, granted here (wrapping arithmetic; correct
    /// for gaps under the u32 tick wrap, ~71 min at 1 MHz — far above any sane
    /// `max_age`).
    #[cfg(feature = "liveness")]
    pub fn ticks_since_beat(&self) -> u32 {
        let now = embassy_time::Instant::now().as_ticks() as u32;
        // A beat since the last look is granted here: the checker pays the
        // (already-read) clock, the beating task never does. Load-then-swap so
        // a plain check stays one load; racing checkers are benign — one
        // stamps, the rest see it stamped.
        if self.handle.pending_beat.load(Ordering::Relaxed)
            && self.handle.pending_beat.swap(false, Ordering::AcqRel)
        {
            self.handle.last_beat.store(now, Ordering::Release);
        }
        now.wrapping_sub(self.handle.last_beat.load(Ordering::Acquire))
    }

    /// Ticks until this node next needs looking at, for the monitor's sleep.
    ///
    /// `None` when the node is unpoliced. Three cases, in the order they matter:
    ///
    /// * **Waiting to assert readiness** (`ready_on_write`, not yet ready) — a
    ///   short probe. Readiness gates dependents' spawns against their
    ///   `slot_timeout`, so noticing the first write late spends someone else's
    ///   budget. Bring-up is brief and latency-sensitive; a fraction of the beat
    ///   budget buys that back and costs nothing once the node is ready.
    /// * **Overdue** — half a budget, so a stalled node's `beat_window` strikes
    ///   accumulate at a bounded rate instead of spinning on a zero delay.
    /// * **Running normally** — exactly when it would go stale.
    ///
    /// A node that is down or detached is re-examined a budget later: there is
    /// nothing to report about it now, but it may be running by then.
    #[cfg(feature = "liveness-monitor")]
    fn ticks_until_check(&self) -> Option<u64> {
        let budget = self.cfg.beat_timeout?.as_ticks();
        if self.is_detached() || !self.is_running() {
            return Some(budget);
        }
        // The probe serves the sweep-driven (`observed`) form only; a verb
        // write asserts readiness inline, with nothing to poll for.
        #[cfg(all(feature = "coupling-observe", feature = "readiness"))]
        if self.cfg.ready_on_write && !self.is_ready() {
            return Some((budget / READY_PROBE_DIVISOR).max(1));
        }
        Some(
            match budget.saturating_sub(self.ticks_since_beat() as u64) {
                0 => (budget / 2).max(1),
                remaining => remaining,
            },
        )
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

    /// This node's activation generation: `0` before the first spawn, then
    /// incremented on every transition into `running` — a fresh spawn, a pool
    /// grow, a `respawn_terminate`, or a `Pause` node's resume.
    ///
    /// **The dependent-side answer to "my provider was restarted underneath
    /// me".** `deps:` gates a spawn once; nothing re-gates a node that is
    /// *already running* when one of its providers cycles. A consumer holding
    /// derived state (a filter, a session, a cached handle) samples this once
    /// and compares it each iteration — one relaxed load, cheap enough for a
    /// 1 kHz loop:
    ///
    /// ```ignore
    /// let mut seen = PROVIDER.epoch();
    /// loop {
    ///     let sample = INPUT.wait().await;
    ///     let now = PROVIDER.epoch();
    ///     if now != seen {
    ///         seen = now;
    ///         filter.reset();   // the provider is a new instance; derived state is stale
    ///     }
    ///     // ...
    /// }
    /// ```
    #[cfg(feature = "epochs")]
    pub fn epoch(&self) -> u32 {
        self.handle.epoch.load(Ordering::Acquire)
    }

    #[cfg(feature = "epochs")]
    /// Wait until the node's epoch counter differs from `seen`.
    pub async fn wait_epoch_change(&self, seen: u32) -> u32 {
        loop {
            let now = self.epoch();
            if now != seen {
                return now;
            }
            self.handle.epoch_wake.wait().await;
        }
    }

    /// Wait until the supervisor signals this Pause-mode node to resume.
    pub async fn wait_resume(&self) {
        self.handle.resume_wake.wait().await;
    }

    /// Run `fut` until it completes or the node is stopped, returning [`Aborted`] on stop.
    pub fn run_cancellable<F: Future>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, Aborted>> {
        RunCancellable {
            fut: Some(fut),
            node: self,
            ack: false,
        }
    }

    /// Like [`run_cancellable`](Self::run_cancellable), but also acks the stop handshake.
    pub fn run_cancellable_acked<F: Future>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, Aborted>> {
        RunCancellable {
            fut: Some(fut),
            node: self,
            ack: true,
        }
    }

    /// Run `fut` until it completes or the node is paused, returning [`Resumed`] on pause.
    pub fn run_pausable<F: Future>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, Resumed>> {
        RunPausable {
            fut: Some(fut),
            node: self,
            parked: false,
        }
    }

    /// Run `body` in a `run_pausable` loop forever, surviving pause/resume cycles.
    pub async fn run_pausable_loop(&self, mut body: impl AsyncFnMut()) -> ! {
        loop {
            let _ = self.run_pausable(body()).await;
        }
    }

    /// Mark this node as busy, requesting pool scale-out if one is configured.
    pub fn mark_busy(&self) {
        if !self.handle.flag_swap(flag::BUSY, true) {
            request_scale();
        }
    }

    /// Mark this node as idle, requesting pool scale-in if one is configured.
    pub fn mark_idle(&self) {
        if self.handle.flag_swap(flag::BUSY, false) {
            request_scale();
        }
    }

    /// Return `true` if the node is currently marked busy.
    pub fn is_busy(&self) -> bool {
        self.handle.flag(flag::BUSY)
    }

    /// Return `true` if the node has a running instance.
    pub fn is_running(&self) -> bool {
        self.handle.flag(flag::RUNNING)
    }

    /// Return `true` if the node has been manually disabled.
    pub fn is_disabled(&self) -> bool {
        self.handle.flag(flag::DISABLED)
    }

    /// Set whether this node is detached from automatic lifecycle management.
    pub fn set_detached(&self, detached: bool) {
        self.handle.flag_put(flag::DETACHED, detached);
    }

    /// Return `true` if the node is detached from automatic lifecycle management.
    pub fn is_detached(&self) -> bool {
        self.handle.flag(flag::DETACHED)
    }

    #[cfg(feature = "trace")]
    /// Record the executor task id for this node instance.
    pub fn set_task_id(&self, id: u32) {
        self.handle.task_id.store(id, Ordering::Release);
    }

    #[cfg(feature = "trace")]
    /// Adopt a spawn token's task id (and name, if enabled) for tracing.
    pub fn adopt<S>(&self, token: &embassy_executor::SpawnToken<S>) {
        self.set_task_id(token.id());
        #[cfg(feature = "metadata-names")]
        self.stamp_name(token);
    }

    #[cfg(feature = "trace")]
    /// Adopt the current task's id for tracing.
    pub async fn adopt_current(&self) {
        self.set_task_id(trace::current_task_id().await);
    }

    #[cfg(feature = "metadata-names")]
    /// Set the spawn token's task name to this node's configured name.
    pub fn stamp_name<S>(&self, token: &embassy_executor::SpawnToken<S>) {
        token.metadata().set_name(self.cfg.name);
    }

    #[cfg(feature = "trace")]
    /// Return the id of the task currently adopted by this node.
    pub fn task_id(&self) -> u32 {
        self.handle.task_id.load(Ordering::Acquire)
    }

    #[cfg(feature = "trace")]
    /// Return the accumulated execution tick count for this node.
    pub fn exec_ticks(&self) -> u32 {
        self.handle.exec_ticks.load(Ordering::Relaxed)
    }

    #[cfg(feature = "trace")]
    /// Return the number of poll cycles recorded for this node.
    pub fn poll_count(&self) -> u32 {
        self.handle.polls.load(Ordering::Relaxed)
    }

    #[cfg(feature = "trace")]
    /// Return the longest single-poll tick count recorded for this node.
    pub fn max_poll_ticks(&self) -> u32 {
        self.handle.max_poll_ticks.load(Ordering::Relaxed)
    }

    pub(crate) fn signal_shutdown(&self) {
        self.handle.flag_set(flag::SHUTDOWN);
        self.handle.shutdown_wake.signal(());
    }

    pub(crate) fn signal_resume(&self) {
        self.handle.resume_wake.signal(());
    }

    pub(crate) fn set_running(&self, running: bool) {
        self.handle.flag_put(flag::RUNNING, running);
        #[cfg(feature = "liveness")]
        if running {
            self.handle.last_beat.store(
                embassy_time::Instant::now().as_ticks() as u32,
                Ordering::Release,
            );
            #[cfg(feature = "coupling-observe")]
            self.seed_write_mark();
        }
        #[cfg(feature = "liveness")]
        if running {
            self.handle.pending_beat.store(false, Ordering::Release);
        }
        #[cfg(feature = "node-status")]
        if running {
            self.handle.status.lock(|s| s.set(None));
        }
        #[cfg(feature = "epochs")]
        if running {
            self.handle.epoch.fetch_add(1, Ordering::AcqRel);
            self.handle.epoch_wake.signal(());
        }
    }

    /// Manually disable or re-enable this node.
    pub fn set_disabled(&self, disabled: bool) {
        self.handle.flag_put(flag::DISABLED, disabled);
    }

    pub(crate) fn has_acked_stop(&self) -> bool {
        self.handle.flag(flag::DROPPED) && !self.handle.flag(flag::COMPLETED)
    }

    pub(crate) async fn wait_dropped(&self) {
        if self.handle.flag(flag::DROPPED) {
            return;
        }
        self.handle.dropped_wake.wait().await;
    }

    pub(crate) fn has_dropped(&self) -> bool {
        self.handle.flag(flag::DROPPED)
    }

    pub(crate) fn reset(&self) {
        let stale = flag::SHUTDOWN | flag::DROPPED | flag::BUSY | flag::COMPLETED;
        #[cfg(feature = "readiness")]
        let stale = stale | flag::READY;
        self.handle.flag_clear(stale);
        #[cfg(feature = "readiness")]
        self.handle.ready_wake.reset();
        self.handle.shutdown_wake.reset();
        self.handle.dropped_wake.reset();
    }
}

impl core::fmt::Debug for TaskNode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_struct("TaskNode");
        d.field("name", &self.cfg.name)
            .field("mode", &self.cfg.mode)
            .field("running", &self.is_running())
            .field("busy", &self.is_busy())
            .field("disabled", &self.is_disabled())
            .field("detached", &self.is_detached());
        #[cfg(feature = "epochs")]
        d.field("epoch", &self.epoch());
        d.finish_non_exhaustive()
    }
}

// ─── Topology ────────────────────────────────────────────────────────────

/// Structural facts about a graph, as bits in [`Topology::SHAPE`] — what the
/// graph *contains*, decided at `supervisor_graph!` expansion and carried in
/// the topology's **type**, so lifecycle code serving an absent structure is
pub mod shape {
    /// The graph contains at least one `ready` dependency.
    pub const READY_DEPS: u32 = 1 << 0;
    /// The graph declares at least one named executor slot.
    pub const EXEC_SLOTS: u32 = 1 << 1;
    /// The graph declares at least one resource slot.
    pub const RESOURCES: u32 = 1 << 2;
    /// The graph contains at least one `Pause` node or pool member.
    pub const PAUSE: u32 = 1 << 3;
    /// The graph contains at least one `OnDemand` node or pool member.
    pub const ON_DEMAND: u32 = 1 << 4;
    /// The graph declares at least one heartbeat (`beat_timeout:` or `beat`).
    pub const BEATS: u32 = 1 << 5;
    /// The graph declares at least one `observed` signal entry.
    pub const OBSERVED: u32 = 1 << 6;
    /// The graph contains at least one `bound` dependency.
    pub const BOUND_DEPS: u32 = 1 << 7;
    /// The graph declares at least one elastic pool.
    pub const POOLS: u32 = 1 << 8;
    /// All shape bits set.
    pub const ALL: u32 = u32::MAX;
}

/// Structural information about a graph, used by [`Supervisor`] to decide
/// which lifecycle code paths can be compiled out.
pub trait Topology<const N: usize>: 'static {
    /// Structural-fact bits (see [`shape`]). An unset bit promises the
    /// structure is absent from the whole graph.
    const SHAPE: u32;

    /// The dependency indices of slot `i` — what node `i` declared it needs
    /// spawned first. **Spawn ordering, not runtime coupling**: see the crate
    /// docs on what a `deps:` edge does and does not assert.
    fn deps_of(&self, i: u8) -> &'static [u8];

    /// Return the slot index at topological position `k` (0..N).
    fn order_at(&self, k: usize) -> u8;
}

/// A [`Topology`] whose nodes are topologically sorted by their `deps:` edges.
pub struct Ordered<const N: usize, const SHAPE: u32> {
    deps: &'static [&'static [u8]; N],
    order: [u8; N],
}

impl<const N: usize, const SHAPE: u32> Ordered<N, SHAPE> {
    /// Build a topology from a static array of dependency lists.
    pub const fn new(deps: &'static [&'static [u8]; N]) -> Self {
        Self {
            deps,
            order: topo_sort_const(deps),
        }
    }
}

impl<const N: usize, const SHAPE: u32> Topology<N> for Ordered<N, SHAPE> {
    const SHAPE: u32 = SHAPE;

    fn deps_of(&self, i: u8) -> &'static [u8] {
        self.deps[i as usize]
    }

    fn order_at(&self, k: usize) -> u8 {
        self.order[k]
    }
}

/// The [`Topology`] of a graph with **no** `deps:` edges anywhere: zero-sized,
/// every dep list is empty by type, and the topological order is declaration
/// order. The walks a [`Supervisor`] runs over it fold to plain index loops,
/// and the dependency cascades (`activate`, `deactivate`, `restart`) collapse
/// to their seed sets.
pub struct Flat<const SHAPE: u32>;

impl<const SHAPE: u32> Flat<SHAPE> {
    /// The (zero-sized) flat topology.
    pub const fn new() -> Self {
        Self
    }
}

impl<const SHAPE: u32> Default for Flat<SHAPE> {
    fn default() -> Self {
        Self
    }
}

impl<const N: usize, const SHAPE: u32> Topology<N> for Flat<SHAPE> {
    const SHAPE: u32 = SHAPE;

    fn deps_of(&self, _i: u8) -> &'static [u8] {
        &[]
    }

    fn order_at(&self, k: usize) -> u8 {
        k as u8
    }
}

const fn has(shape_bits: u32, bit: u32) -> bool {
    shape_bits & bit != 0
}

/// A static task graph: the nodes, the topology over them, and optional pools.
pub struct Graph<const N: usize, T: Topology<N> = Ordered<N, { shape::ALL }>> {
    /// The fixed array of node slots; `None` marks a disabled or cfg-gapped slot.
    pub nodes: &'static [Option<&'static TaskNode>; N],
    /// The topology that defines spawn order and dependency rows.
    pub topo: T,
    #[cfg(feature = "pool")]
    /// The elastic pools declared in the graph.
    pub pools: &'static [&'static dyn Pool],
    #[cfg(feature = "graph-ref")]
    /// A reference used to enumerate the graph at runtime.
    pub graph_ref: &'static GraphRef,
}

impl<const N: usize, T: Topology<N>> Graph<N, T> {
    /// Slot index of `node` in this graph (pointer identity — every node is a
    /// `&'static`), or `None` if it belongs to another graph. The inverse of
    /// indexing [`nodes`](Self::nodes), and the bridge an app-side health view
    /// needs to get from a node back to its [`deps_of`](Self::deps_of) row.
    pub fn index_of(&self, node: &'static TaskNode) -> Option<u8> {
        self.nodes
            .iter()
            .position(|s| s.is_some_and(|n| core::ptr::eq(n, node)))
            .map(|i| i as u8)
    }

    /// The dependency indices of slot `i` — what node `i` declared it needs
    /// spawned first. **Spawn ordering, not runtime coupling**: see the crate
    /// docs on what a `deps:` edge does and does not assert.
    ///
    /// # Panics
    /// If `i >= N` (on an [`Ordered`] topology; [`Flat`] has no rows to index).
    pub fn deps_of(&self, i: u8) -> &'static [u8] {
        self.topo.deps_of(i)
    }

    /// The slot indices in topological order (dependencies before their
    /// dependents; `.rev()` is the teardown order). Declaration order on a
    /// [`Flat`] topology.
    pub fn order(&self) -> impl DoubleEndedIterator<Item = u8> + ExactSizeIterator + '_ {
        (0..N).map(|k| self.topo.order_at(k))
    }

    /// Call `visit` with the slot index of every node that declares slot `i` as
    /// a dependency (direct dependents only). Computed by a forward scan of
    /// the dep rows — no reverse-edge table is stored, so this is
    /// O(N·E) and meant for control paths and status endpoints, not hot loops.
    ///
    /// `&mut dyn FnMut` rather than a generic: one instantiation, no
    /// monomorphization per call site.
    pub fn dependents_of(&self, i: u8, visit: &mut dyn FnMut(u8)) {
        for j in 0..N {
            if self.topo.deps_of(j as u8).contains(&i) {
                visit(j as u8);
            }
        }
    }

    /// Iterate the live nodes with their slot indices, skipping `#[cfg]`-ed-out
    /// slots — the ergonomic form of `GRAPH.nodes.iter().enumerate()` for a
    /// status endpoint that needs the index (to reach `deps`) alongside the node.
    pub fn iter_nodes(&self) -> impl Iterator<Item = (u8, &'static TaskNode)> + '_ {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|n| (i as u8, n)))
    }

    /// Every node declaring `signal` in its `writes:`, by slot and node.
    /// Matched by address, so it is the *static* that is compared, not the
    /// path text.
    #[cfg(feature = "coupling")]
    pub fn writers_of(&self, signal: &Coupling, visit: &mut dyn FnMut(u8, &'static TaskNode)) {
        for (i, node) in self.iter_nodes() {
            if node.has_entry(signal, true) {
                visit(i, node);
            }
        }
    }

    /// Every node declaring `signal` in its `reads:`. The counterpart to
    /// [`writers_of`](Self::writers_of); together they answer the structural
    /// questions about one signal — who produces it, who consumes it, which
    /// pairs are coupled in a loop.
    #[cfg(feature = "coupling")]
    pub fn readers_of(&self, signal: &Coupling, visit: &mut dyn FnMut(u8, &'static TaskNode)) {
        for (i, node) in self.iter_nodes() {
            if node.has_entry(signal, false) {
                visit(i, node);
            }
        }
    }
}

// ─── Supervisor ──────────────────────────────────────────────────────────

/// Orchestrates a set of managed tasks across spawn / teardown / bring-up.
///
/// Owned by a single supervisor task. Concurrent access from other tasks goes
/// through each [`TaskNode`]'s own atomic state, not the `Supervisor` struct.
pub struct Supervisor<const N: usize, T: Topology<N> = Ordered<N, { shape::ALL }>> {
    /// Node slots, one per declared node. `None` marks a slot whose node was
    /// `#[cfg]`-ed out of the build (feature-gated); every method skips those.
    nodes: &'static [Option<&'static TaskNode>; N],
    /// The graph's [`Topology`]: the dep rows and topological order every walk
    /// iterates (reverse iteration is the teardown order), and the structural
    /// [`shape`] bits the gates fold on. Borrowed from the `static` [`Graph`]
    /// rather than copied: a `Supervisor` usually lives inside a task future
    /// (i.e. in that task's `static` storage), so an inline order array would
    /// cost N bytes of RAM per supervisor plus the copy code for no benefit —
    /// and for [`Flat`] this reference is the field's entire cost.
    topo: &'static T,
    /// Elastic pools, so the control interface can co-control a whole pool from
    /// any one member (`apply_control` expands the target through
    /// [`Pool::members`]) — the same registry `run_pools` drives. Taken from
    /// `GRAPH.pools` at construction (empty when no pool is declared).
    #[cfg(feature = "pool")]
    pools: &'static [&'static dyn Pool],
    /// This graph as one `'static`, linked into the binary-wide chain by
    /// [`start`](Supervisor::start) so the trace hooks can resolve a task id to
    /// one of its nodes.
    #[cfg(feature = "trace")]
    graph_ref: &'static GraphRef,
}

async fn await_spawn_slot(node: &'static TaskNode) -> Result<(), NodeFault> {
    if let Some(slot) = node.cfg.spawn_slot {
        with_timeout(node.slot_timeout(), slot.ready())
            .await
            .map_err(|_| NodeFault {
                node,
                kind: FaultKind::ExecutorSlotEmpty,
            })?;
    }
    Ok(())
}

/// Await every [`ResourceSlot`] a node's `resources:` clause takes from being
async fn await_resources(node: &'static TaskNode) -> Result<(), NodeFault> {
    for gate in node.cfg.resource_gates {
        let wait = async {
            loop {
                if gate.is_filled() {
                    break;
                }
                gate.filled_signal().wait().await;
            }
        };
        with_timeout(node.slot_timeout(), wait)
            .await
            .map_err(|_| NodeFault {
                node,
                kind: FaultKind::ResourceMissing,
            })?;
    }
    Ok(())
}

/// Await every `ready`-marked dep's task-asserted readiness before spawning
#[cfg(feature = "readiness")]
async fn await_ready_deps(node: &'static TaskNode) -> Result<(), NodeFault> {
    for dep in node.ready_deps() {
        if with_timeout(node.slot_timeout(), dep.wait_ready())
            .await
            .is_err()
        {
            return Err(NodeFault {
                node,
                kind: FaultKind::ReadyDepTimeout { dep },
            });
        }
    }
    Ok(())
}
#[cfg(not(feature = "readiness"))]
async fn await_ready_deps(_node: &'static TaskNode) -> Result<(), NodeFault> {
    Ok(())
}

impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    const NODE_CAP: () = assert!(N <= 256, "supervisor: a graph holds at most 256 nodes");

    /// Create a supervisor from a statically-built graph.
    pub const fn new(graph: &'static Graph<N, T>) -> Self {
        let () = Self::NODE_CAP;
        Self {
            nodes: graph.nodes,
            topo: &graph.topo,
            #[cfg(feature = "pool")]
            pools: graph.pools,
            #[cfg(feature = "trace")]
            graph_ref: graph.graph_ref,
        }
    }

    /// Does this graph's shape carry `bit` (see [`shape`])? `T::SHAPE` is a
    #[inline(always)]
    fn has(bit: u32) -> bool {
        has(T::SHAPE, bit)
    }

    fn order_iter(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        (0..N).map(|k| self.topo.order_at(k) as usize)
    }

    /// The pre-spawn gate sequence — executor slot, resource gates, `ready`
    /// deps — with each wait compiled out when the graph's shape lacks the
    async fn await_gates(node: &'static TaskNode) -> Result<(), NodeFault> {
        if Self::has(shape::EXEC_SLOTS) {
            await_spawn_slot(node).await?;
        }
        if Self::has(shape::RESOURCES) {
            await_resources(node).await?;
        }
        if Self::has(shape::READY_DEPS) {
            await_ready_deps(node).await?;
        }
        Ok(())
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
    pub async fn start(&self, spawner: &Spawner) -> Result<(), NodeFault> {
        #[cfg(feature = "trace")]
        self.graph_ref.register();

        #[cfg(feature = "trace-self")]
        if let Some(node) = self.graph_ref.self_node() {
            node.set_task_id(trace::current_task_id().await);
            node.handle.flag_set(flag::RUNNING | flag::DETACHED);
        }

        self.start_nodes(
            spawner,
            &mut |_, node| {
                !(Self::has(shape::ON_DEMAND) && matches!(node.mode(), Mode::OnDemand))
                    && !node.is_disabled()
                    && !node.is_running()
                    && !node.is_detached()
            },
            false,
        )
        .await
    }

    #[cfg(any(feature = "pool", feature = "control"))]
    /// Run this node to completion, handling start, driver, and monitoring.
    pub async fn run(&self, spawner: &Spawner) -> NodeFault {
        if let Err(e) = self.start(spawner).await {
            return e;
        }
        #[cfg(feature = "liveness-monitor")]
        match select(self.run_driver(spawner), self.monitor()).await {
            Either::First(e) => e,
            Either::Second(never) => match never {},
        }
        #[cfg(not(feature = "liveness-monitor"))]
        self.run_driver(spawner).await
    }

    #[cfg(any(feature = "pool", feature = "control"))]
    async fn run_driver(&self, spawner: &Spawner) -> NodeFault {
        #[cfg(feature = "bound-deps")]
        return match select(self.run_driver_inner(spawner), self.run_binds(spawner)).await {
            Either::First(e) | Either::Second(e) => e,
        };
        #[cfg(not(feature = "bound-deps"))]
        self.run_driver_inner(spawner).await
    }

    #[cfg(all(feature = "bound-deps", any(feature = "pool", feature = "control")))]
    async fn run_binds(&self, spawner: &Spawner) -> NodeFault {
        loop {
            wait_bind().await;
            if let Err(e) = self.apply_bind(spawner).await {
                return e;
            }
        }
    }

    #[cfg(any(feature = "pool", feature = "control"))]
    async fn run_driver_inner(&self, spawner: &Spawner) -> NodeFault {
        #[cfg(all(feature = "pool", feature = "control"))]
        loop {
            match select(self.run_pools(spawner), wait_control()).await {
                Either::First(e) => return e,
                Either::Second(cmd) => {
                    if let Err(e) = self.apply_control(cmd, spawner).await {
                        return e;
                    }
                }
            }
        }
        #[cfg(all(feature = "pool", not(feature = "control")))]
        return self.run_pools(spawner).await;
        #[cfg(all(feature = "control", not(feature = "pool")))]
        loop {
            let cmd = wait_control().await;
            if let Err(e) = self.apply_control(cmd, spawner).await {
                return e;
            }
        }
    }

    #[cfg(feature = "liveness-monitor")]
    /// Monitor node liveness beats and restart nodes that miss deadlines.
    pub async fn monitor(&self) -> core::convert::Infallible {
        if !Self::has(shape::BEATS)
            || !self
                .nodes
                .iter()
                .flatten()
                .any(|n| n.beat_timeout().is_some())
        {
            info!("supervisor: liveness monitor idle (no node declares beat_timeout)");
            let never: core::convert::Infallible = core::future::pending().await;
            match never {}
        }

        loop {
            let sleep = self
                .nodes
                .iter()
                .flatten()
                .filter_map(|n| n.ticks_until_check())
                .min()
                .unwrap_or(1);
            Timer::after(embassy_time::Duration::from_ticks(sleep)).await;

            for node in self.nodes.iter().flatten() {
                let Some(budget) = node.beat_timeout() else {
                    continue;
                };
                if node.is_detached() || !node.is_running() {
                    node.handle.stale_strikes.store(0, Ordering::Release);
                    continue;
                }

                #[cfg(feature = "coupling-observe")]
                if Self::has(shape::OBSERVED) && node.poll_observed_writes() {
                    node.beat();
                    #[cfg(feature = "readiness")]
                    if node.ready_on_write() && !node.is_ready() {
                        node.set_ready();
                    }
                }

                if node.is_stale(budget) {
                    let window = node.beat_window().max(1);
                    let strikes = node.handle.stale_strikes.load(Ordering::Acquire);
                    if strikes < window {
                        let strikes = strikes + 1;
                        node.handle.stale_strikes.store(strikes, Ordering::Release);
                        if strikes == window {
                            warn!(
                                "supervisor: {} has not beaten in {} ticks",
                                node.name(),
                                node.ticks_since_beat()
                            );
                            emit_health(HealthEvent {
                                node,
                                kind: HealthKind::Stale {
                                    ticks: node.ticks_since_beat(),
                                },
                            });
                        }
                    }
                } else {
                    let had = node.handle.stale_strikes.swap(0, Ordering::AcqRel);
                    if had >= node.beat_window().max(1) {
                        info!("supervisor: {} is beating", node.name());
                        emit_health(HealthEvent {
                            node,
                            kind: HealthKind::Recovered,
                        });
                    }
                }
            }
        }
    }

    /// Start a single node if it is not already running and not detached.
    pub async fn start_node(
        &self,
        node: &'static TaskNode,
        spawner: &Spawner,
    ) -> Result<(), NodeFault> {
        node.reset();
        if let Some(spawn) = node.cfg.spawn {
            Self::await_gates(node).await?;
            let mut result = spawn(*spawner);
            if result.is_err() {
                // A just-stopped instance's storage frees one executor pass
                embassy_futures::yield_now().await;
                result = spawn(*spawner);
            }
            result.map_err(|err| NodeFault {
                node,
                kind: FaultKind::Spawn(err),
            })?;
        }
        node.set_running(true);
        info!("supervisor: started {}", node.name());
        Ok(())
    }

    async fn shutdown_and_wait(&self, node: &'static TaskNode) -> Result<(), NodeFault> {
        node.signal_shutdown();
        self.await_ack(node).await
    }

    /// The waiting half of [`shutdown_and_wait`](Self::shutdown_and_wait), for
    /// a caller that signalled the node earlier. Returns immediately for a node
    /// that already acked.
    async fn await_ack(&self, node: &'static TaskNode) -> Result<(), NodeFault> {
        if let Either::Second(()) =
            select(node.wait_dropped(), Timer::after(node.ack_timeout())).await
        {
            warn!(
                "supervisor: task {} did not ack shutdown within {}ms",
                node.name(),
                node.ack_timeout().as_millis(),
            );
            return Err(NodeFault {
                node,
                kind: FaultKind::ShutdownTimeout,
            });
        }
        node.set_running(false);
        Ok(())
    }

    async fn stop_nodes(
        &self,
        select: &mut dyn FnMut(usize, &'static TaskNode) -> bool,
        keep_going: bool,
    ) -> Result<(), NodeFault> {
        let mut chosen = [false; N];
        for j in self.order_iter().rev() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if select(j, node) {
                chosen[j] = true;
            }
        }

        self.stop_wave(&chosen, keep_going).await
    }

    /// The down direction: signal each chosen node the moment every *chosen
    /// dependent* of it has resolved, parking on the
    /// ack event ([`STOP_EVT`]) between rounds. Two guarantees at once, which
    /// signalling everything up front could not give together:
    ///
    ///   * a `deps:` dependency keeps serving until its stopping dependents
    ///     have acked, so a dependent may flush over a link — or drive one
    ///     last ioctl through a runner it depends on — during its own
    ///     shutdown;
    ///   * a node whose shutdown waits on a node it has NO edge to (a
    ///     producer draining a [`Leased`] signal) is never kept waiting on
    ///     the wave's own progress: every node without an unresolved chosen
    async fn stop_wave(&self, chosen: &[bool; N], keep_going: bool) -> Result<(), NodeFault> {
        const UNSIGNALED: u32 = u32::MAX;
        let epoch = embassy_time::Instant::now();
        let mut resolved = [false; N];
        let mut signaled: [u32; N] = [UNSIGNALED; N];
        let mut first_err = Ok(());
        loop {
            for j in self.order_iter().rev() {
                if !chosen[j] || resolved[j] || signaled[j] != UNSIGNALED {
                    continue;
                }
                let held = (0..N).any(|k| {
                    chosen[k] && !resolved[k] && self.topo.deps_of(k as u8).contains(&(j as u8))
                });
                if held {
                    continue;
                }
                self.nodes[j]
                    .expect("a chosen slot is occupied")
                    .signal_shutdown();
                signaled[j] = ((embassy_time::Instant::now() - epoch).as_millis())
                    .min(UNSIGNALED as u64 - 1) as u32;
            }

            let mut progress = false;
            for j in self.order_iter().rev() {
                if !chosen[j] || resolved[j] {
                    continue;
                }
                let node = self.nodes[j].expect("a chosen slot is occupied");
                if node.has_dropped() {
                    node.set_running(false);
                    resolved[j] = true;
                    progress = true;
                }
            }
            if (0..N).all(|j| !chosen[j] || resolved[j]) {
                return first_err;
            }
            if progress {
                continue;
            }

            let now_ms = (embassy_time::Instant::now() - epoch).as_millis();
            let mut deadline: Option<embassy_time::Instant> = None;
            for j in self.order_iter().rev() {
                if !chosen[j] || resolved[j] || signaled[j] == UNSIGNALED {
                    continue;
                }
                let node = self.nodes[j].expect("a chosen slot is occupied");
                let due_ms = signaled[j] as u64 + node.ack_timeout().as_millis();
                if now_ms < due_ms {
                    let due = epoch + embassy_time::Duration::from_millis(due_ms);
                    deadline = Some(deadline.map_or(due, |d| d.min(due)));
                    continue;
                }
                warn!(
                    "supervisor: task {} did not ack shutdown within {}ms",
                    node.name(),
                    node.ack_timeout().as_millis(),
                );
                let fault = NodeFault {
                    node,
                    kind: FaultKind::ShutdownTimeout,
                };
                if !keep_going {
                    return Err(fault);
                }
                if first_err.is_ok() {
                    first_err = Err(fault);
                }
                resolved[j] = true;
                progress = true;
            }
            if progress {
                continue;
            }
            let deadline = deadline.expect("an unresolved wave has a signalled node");
            let _ = embassy_futures::select::select(STOP_EVT.wait(), Timer::at(deadline)).await;
        }
    }

    async fn start_nodes(
        &self,
        spawner: &Spawner,
        select: &mut dyn FnMut(usize, &'static TaskNode) -> bool,
        keep_going: bool,
    ) -> Result<(), NodeFault> {
        let mut pending = [false; N];
        for j in self.order_iter() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if select(j, node) {
                pending[j] = true;
            }
        }

        // When each node's deps resolved — the start of its gate budget, and
        const UNARMED: u32 = u32::MAX;
        let epoch = embassy_time::Instant::now();
        let mut armed: [u32; N] = [UNARMED; N];
        let mut first_err = Ok(());
        loop {
            let mut progress = false;
            let mut waiting = false;
            let mut respawn_wait = false;
            'nodes: for j in self.order_iter() {
                if !pending[j] {
                    continue;
                }
                let node = self.nodes[j].expect("a pending slot is occupied");
                if self
                    .topo
                    .deps_of(j as u8)
                    .iter()
                    .any(|&d| pending[d as usize])
                {
                    waiting = true;
                    continue;
                }
                let budget_start = if armed[j] != UNARMED {
                    armed[j]
                } else {
                    {
                        let now = ((embassy_time::Instant::now() - epoch).as_millis())
                            .min(UNARMED as u64 - 1) as u32;
                        armed[j] = now;
                        // Parked `Pause` instance: resume in place, before the
                        // reset that clears the ack flags this reads — and
                        // deliberately without the gate waits, since the
                        // parked instance retains its resources.
                        if Self::has(shape::PAUSE)
                            && matches!(node.mode(), Mode::Pause)
                            && node.has_acked_stop()
                        {
                            node.reset();
                            info!("supervisor: resuming {} in place", node.name());
                            node.signal_resume();
                            node.set_running(true);
                            pending[j] = false;
                            progress = true;
                            continue;
                        }
                        node.reset();
                        info!("supervisor: spawning {} ({})", node.name(), node.mode());
                        now
                    }
                };
                let Some(spawn) = node.cfg.spawn else {
                    // A parked node the app spawns itself; only marked.
                    node.set_running(true);
                    pending[j] = false;
                    progress = true;
                    continue;
                };
                // The gate sequence — executor slot, resources, ready deps —
                // tested without blocking; the first unsatisfied gate defers
                // the node to the next round, or faults it once its budget is
                // spent.
                let overdue = (embassy_time::Instant::now() - epoch).as_millis()
                    >= budget_start as u64 + node.slot_timeout().as_millis();
                let mut unsatisfied = |kind: FaultKind| -> Result<bool, NodeFault> {
                    if !overdue {
                        return Ok(true);
                    }
                    let fault = NodeFault { node, kind };
                    if !keep_going {
                        return Err(fault);
                    }
                    warn!("supervisor: {}", fault);
                    if first_err.is_ok() {
                        first_err = Err(fault);
                    }
                    Ok(false)
                };
                let blocked = if Self::has(shape::EXEC_SLOTS)
                    && node.cfg.spawn_slot.is_some_and(|s| s.get().is_none())
                {
                    Some(unsatisfied(FaultKind::ExecutorSlotEmpty)?)
                } else if Self::has(shape::RESOURCES)
                    && node.cfg.resource_gates.iter().any(|g| !g.is_filled())
                {
                    Some(unsatisfied(FaultKind::ResourceMissing)?)
                } else {
                    None
                };
                #[cfg(feature = "readiness")]
                let blocked = match blocked {
                    Some(b) => Some(b),
                    None if Self::has(shape::READY_DEPS) => {
                        match node.ready_deps().iter().find(|d| !d.is_ready()) {
                            Some(dep) => Some(unsatisfied(FaultKind::ReadyDepTimeout { dep })?),
                            None => None,
                        }
                    }
                    None => None,
                };
                match blocked {
                    Some(true) => {
                        waiting = true;
                        continue 'nodes;
                    }
                    Some(false) => {
                        pending[j] = false;
                        progress = true;
                        continue 'nodes;
                    }
                    None => {}
                }
                if let Err(err) = spawn(*spawner) {
                    // A respawn can catch the previous instance's storage
                    if unsatisfied(FaultKind::Spawn(err))? {
                        respawn_wait = true;
                        waiting = true;
                    } else {
                        pending[j] = false;
                        progress = true;
                    }
                    continue;
                }
                node.set_running(true);
                pending[j] = false;
                progress = true;
            }
            if !waiting {
                return first_err;
            }
            if progress {
                continue;
            }
            if respawn_wait {
                embassy_futures::yield_now().await;
                continue;
            }
            let deadline_ms = self
                .order_iter()
                .filter(|&j| pending[j] && armed[j] != UNARMED)
                .map(|j| {
                    armed[j] as u64
                        + self.nodes[j]
                            .expect("a pending slot is occupied")
                            .slot_timeout()
                            .as_millis()
                })
                .min()
                .expect("a waiting wave has an armed node");
            let deadline = epoch + embassy_time::Duration::from_millis(deadline_ms);
            let _ = embassy_futures::select::select(GATE_EVT.wait(), Timer::at(deadline)).await;
        }
    }

    /// Stop a single node, waiting for its shutdown ack.
    pub async fn stop_node(&self, node: &'static TaskNode) -> Result<(), NodeFault> {
        if !node.is_running() || node.is_detached() {
            return Ok(());
        }
        self.shutdown_and_wait(node).await?;
        info!("supervisor: stopped {}", node.name());
        Ok(())
    }

    /// Stop every **running** node, dependents before their dependencies.
    /// Down `OnDemand` nodes are skipped (no instance to ack). Pause-mode nodes
    /// ack and park on `wait_resume()`; Terminate/OnDemand nodes exit.
    ///
    /// The stop runs as a wave: a node is signalled once every dependent
    /// stopping with it has acked — a `deps:` dependency keeps serving
    /// through its dependents' cleanup — and a node with no such dependents
    pub async fn teardown(&self) -> Result<(), NodeFault> {
        self.stop_nodes(
            &mut |_, node| {
                if !node.is_running() || node.is_detached() {
                    return false;
                }
                info!("supervisor: tearing down {}", node.name());
                true
            },
            false,
        )
        .await
    }

    /// Like [`teardown`](Self::teardown), but do not clear the shutdown flags
    /// so a later [`respawn_terminate`](Self::respawn_terminate) can restart.
    pub async fn teardown_continue(&self) -> Result<(), NodeFault> {
        self.stop_nodes(
            &mut |_, node| {
                if !node.is_running() || node.is_detached() {
                    return false;
                }
                info!("supervisor: tearing down {}", node.name());
                true
            },
            true,
        )
        .await
    }

    /// Resume a single parked Pause-mode node.
    pub fn resume_node(&self, node: &'static TaskNode) {
        if !Self::has(shape::PAUSE)
            || !matches!(node.mode(), Mode::Pause)
            || node.is_disabled()
            || node.is_detached()
            || !node.has_acked_stop()
        {
            return;
        }
        node.reset();
        info!("supervisor: resuming {}", node.name());
        node.signal_resume();
        node.set_running(true);
    }

    /// Signal every **parked** Pause-mode node to resume. Cheap and synchronous —
    /// the tasks were parked on `wait_resume()` and pick up immediately. Called
    /// separately from `respawn_terminate` so the application can fire resume
    /// independently of the respawn step. Disabled (manually-paused) nodes are
    /// skipped so a manual pause sticks, detached (self-managed) Pause nodes are
    /// left parked, and — as in [`resume_node`](Self::resume_node) — a node
    /// without a parked instance (`has_acked_stop`) is skipped: signaling it
    /// would latch `resume_wake` with no waiter, and the node's *next* park
    pub fn resume_pausable(&self) {
        if !Self::has(shape::PAUSE) {
            return;
        }
        for j in self.order_iter() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if matches!(node.mode(), Mode::Pause)
                && !node.is_disabled()
                && !node.is_detached()
                && node.has_acked_stop()
            {
                node.reset();
                info!("supervisor: resuming {}", node.name());
                node.signal_resume();
                node.set_running(true);
            }
        }
    }

    /// Restart every terminated Terminate-mode node that is not running,
    /// disabled, or detached.
    pub async fn respawn_terminate(&self, spawner: &Spawner) -> Result<(), NodeFault> {
        self.start_nodes(
            spawner,
            &mut |_, node| {
                matches!(node.mode(), Mode::Terminate)
                    && !node.is_disabled()
                    && !node.is_detached()
                    && !node.is_running()
            },
            false,
        )
        .await
    }
}

#[cfg(any(feature = "control", feature = "pool"))]
impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    fn index_of(&self, node: &'static TaskNode) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| n.is_some_and(|x| core::ptr::eq(x, node)))
    }

    /// Whether every dependency of `node` is currently running, resolved through
    /// the graph's index table. The pool driver checks this before growing a
    #[cfg(feature = "pool")]
    pub(crate) fn deps_running(&self, node: &'static TaskNode) -> bool {
        match self.index_of(node) {
            Some(i) => self
                .topo
                .deps_of(i as u8)
                .iter()
                .all(|&di| self.nodes[di as usize].is_some_and(|n| n.is_running())),
            None => false,
        }
    }
}

#[cfg(feature = "control")]
impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    /// Seed a membership set with `target` plus — if `target` belongs to an
    /// elastic pool — every member of that pool, so control is applied to the
    /// whole pool atomically. Pool membership is read from `GRAPH.pools`; with no
    /// pools (the `pool` feature off, or none declared) this is just `{target}`.
    fn seed(&self, target: &'static TaskNode, set: &mut [bool; N]) {
        if let Some(i) = self.index_of(target) {
            set[i] = true;
        }
        #[cfg(feature = "pool")]
        if Self::has(shape::POOLS) {
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
    }

    fn collect_dependents(&self, set: &mut [bool; N]) {
        for j in self.order_iter() {
            if set[j] {
                continue;
            }
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if node.is_detached() {
                continue;
            }
            if self
                .topo
                .deps_of(j as u8)
                .iter()
                .any(|&di| set[di as usize])
            {
                set[j] = true;
            }
        }
    }

    /// Apply a control command to the requested node.
    pub async fn apply_control(
        &self,
        cmd: ControlCommand,
        spawner: &Spawner,
    ) -> Result<(), NodeFault> {
        match cmd.op {
            ControlOp::Deactivate => self.deactivate(cmd.node).await,
            ControlOp::Activate => {
                self.activate(cmd.node, spawner).await;
                Ok(())
            }
            #[cfg(feature = "restart")]
            ControlOp::Restart => match self.restart(cmd.node, spawner).await {
                Ok(()) => Ok(()),
                Err(e) if matches!(e.kind, FaultKind::ShutdownTimeout) => Err(e),
                Err(e) => {
                    let node = e.node;
                    warn!(
                        "supervisor: {} did not come back after restart",
                        node.name()
                    );
                    Ok(())
                }
            },
            #[allow(unreachable_patterns)]
            _ => Ok(()),
        }
    }

    /// Deactivate the target node and all of its dependents.
    pub async fn deactivate(&self, target: &'static TaskNode) -> Result<(), NodeFault> {
        let mut set = [false; N];
        self.seed(target, &mut set);
        self.collect_dependents(&mut set);

        // Down in reverse topo order (dependents before their deps).
        self.stop_nodes(
            &mut |j, node| {
                if !set[j] {
                    return false;
                }
                // A detached node is self-managed — never control-stop it. The growth
                // loop keeps detached *dependents* out of the set; this also covers a
                // detached node that was seeded directly (or a detached pool member).
                // Without it a detached one-shot that already exited (stale
                // `is_running`, no ack path) would be signalled a shutdown it can never
                // acknowledge, failing here with a spurious missed-ack fault.
                if node.is_detached() {
                    return false;
                }
                // Set on every node in the set, running or not: the flag is what makes
                // the stop stick against the elastic policy and the wake respawn.
                node.set_disabled(true);
                if !node.is_running() {
                    return false;
                }
                info!("supervisor: control-stop {}", node.name());
                true
            },
            false,
        )
        .await
    }

    /// Bring `target` (and its pool, and every transitive dependency) up, in
    /// topological order so each dependency starts before its dependent — the
    /// cascading "turn this subsystem on" verb, and the entry half of the
    /// subordinate sub-graph pattern's one-graph variant: `activate` on a
    pub async fn activate(&self, target: &'static TaskNode, spawner: &Spawner) {
        let mut set = [false; N];
        self.seed(target, &mut set);

        // Grow the set to include transitive deps. Walk dependents-first
        // (reverse topo); when a set member is seen, pull in its direct deps.
        // A detached member's `deps:` are start-ordering only (the node is
        for j in self.order_iter().rev() {
            if set[j] && !self.nodes[j].is_some_and(|n| n.is_detached()) {
                for &di in self.topo.deps_of(j as u8) {
                    set[di as usize] = true;
                }
            }
        }

        let _ = self
            .start_nodes(
                spawner,
                &mut |j, node| {
                    if !set[j] || node.is_detached() {
                        return false;
                    }
                    node.set_disabled(false);
                    !node.is_running()
                        && !(Self::has(shape::ON_DEMAND) && matches!(node.mode(), Mode::OnDemand))
                },
                true,
            )
            .await;
    }
}

#[cfg(feature = "bound-deps")]
fn is_serving(node: &TaskNode) -> bool {
    node.is_running() && node.is_ready()
}

#[cfg(feature = "bound-deps")]
impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    /// React to bound-dependency readiness changes by stopping or resuming nodes.
    pub async fn apply_bind(&self, spawner: &Spawner) -> Result<(), NodeFault> {
        if !Self::has(shape::BOUND_DEPS) {
            return Ok(());
        }
        let mut stopping = [false; N];
        for j in self.order_iter() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if node.is_detached() || !node.is_running() {
                continue;
            }
            let mut down = false;
            for d in node.bound_deps() {
                if !is_serving(d) {
                    down = true;
                    break;
                }
                for (k, slot) in self.nodes.iter().enumerate() {
                    if stopping[k] && slot.is_some_and(|x| core::ptr::eq(x, *d)) {
                        down = true;
                        break;
                    }
                }
                if down {
                    break;
                }
            }
            stopping[j] = down;
        }

        self.stop_nodes(
            &mut |j, node| {
                if !stopping[j] {
                    return false;
                }
                info!(
                    "supervisor: bound-stop {} (a bound provider withdrew readiness)",
                    node.name()
                );
                node.handle.flag_set(flag::BOUND_STOPPED);
                true
            },
            false,
        )
        .await?;

        for j in self.order_iter() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if !node.handle.flag(flag::BOUND_STOPPED) {
                continue;
            }
            if node.is_disabled() || node.is_detached() || node.is_running() {
                continue;
            }
            if !node.bound_deps().iter().all(|d| is_serving(d)) {
                continue;
            }
            match node.mode() {
                Mode::Terminate => {
                    info!("supervisor: bound-restart {}", node.name());
                    match self.start_node(node, spawner).await {
                        Ok(()) => node.handle.flag_clear(flag::BOUND_STOPPED),
                        Err(_) => warn!("supervisor: {} could not be bound-restarted", node.name()),
                    }
                }
                Mode::Pause if Self::has(shape::PAUSE) => {
                    info!("supervisor: bound-resume {}", node.name());
                    node.reset();
                    node.signal_resume();
                    node.set_running(true);
                    node.handle.flag_clear(flag::BOUND_STOPPED);
                }
                Mode::Pause => {}
                Mode::OnDemand => node.handle.flag_clear(flag::BOUND_STOPPED),
            }
        }
        Ok(())
    }
}

#[cfg(feature = "restart")]
impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    /// Restart the target node and all of its dependents.
    pub async fn restart(
        &self,
        target: &'static TaskNode,
        spawner: &Spawner,
    ) -> Result<(), NodeFault> {
        let mut set = [false; N];
        self.seed(target, &mut set);
        self.collect_dependents(&mut set);

        info!(
            "supervisor: restarting {} and its dependents",
            target.name()
        );

        // Down, dependents first. No `disabled` latch: this is a cycle, not a
        // stop, and a concurrent observer must never see the subtree as
        // deliberately disabled.
        self.stop_nodes(
            &mut |j, node| {
                if !set[j] || node.is_detached() || !node.is_running() {
                    return false;
                }
                info!("supervisor: stopping {}", node.name());
                true
            },
            false,
        )
        .await?;

        // Up, dependencies first, each through the full gate sequence — as a
        // wave, aborting on the first fault.
        self.start_nodes(
            spawner,
            &mut |j, node| {
                set[j]
                    && !node.is_detached()
                    && !node.is_disabled()
                    && !node.is_running()
                    && !(Self::has(shape::ON_DEMAND) && matches!(node.mode(), Mode::OnDemand))
            },
            false,
        )
        .await
    }
}

// ─── Topological sort (Kahn's algorithm, const) ───────────────────────────

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

#[cfg(feature = "dataflow")]
mod dataflow;
#[cfg(feature = "dataflow")]
pub use dataflow::*;

#[cfg(feature = "data-deps")]
mod data_deps;
#[cfg(feature = "data-deps")]
pub use data_deps::*;

#[cfg(feature = "graph-ref")]
mod graph_ref;
#[cfg(feature = "graph-ref")]
pub use graph_ref::*;

#[cfg(feature = "trace")]
/// Runtime tracing hooks and task introspection helpers.
pub mod trace;

#[cfg(feature = "macros")]
pub use embassy_supervisor_macros::supervisor_fragment;
#[cfg(feature = "macros")]
pub use embassy_supervisor_macros::supervisor_graph;

#[cfg(feature = "macros")]
#[macro_export]
/// Compose a graph out of one or more `supervisor_fragment!` declarations.
///
/// The fragments are spliced into the final graph at the compose site. This
/// macro is re-exported by `embassy-supervisor` when the `macros` feature is
/// enabled.
macro_rules! compose_graph {
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

#[doc(hidden)]
pub mod _export {
    pub use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
    pub use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    pub use embassy_sync::signal::Signal;
    pub use embassy_time::Duration;
}
