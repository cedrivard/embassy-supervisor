//! A task node: its declared config, its live handle and the task-side
//! protocol.

use super::*;

/// Default per-node timeout for a stop's ack wait (`ack_timeout:` in the graph
/// overrides it per node). A task that doesn't ack within its window is a bug
/// (a body that never notices the stop) or a wedge; the shutdown paths
/// surface it as a [`NodeFault`] naming the node, and the application decides
/// the escalation. 2 s comfortably exceeds a typical task's poll period and
/// peripheral settle time.
const SHUTDOWN_ACK_TIMEOUT_MS: u64 = 2_000;

/// How much finer than its beat budget a `ready_on_write` node is probed while
/// it has yet to assert, capped at the same fraction of the default
/// `slot_timeout` for the activation's first `SLOT_READY_TIMEOUT`: the added
/// readiness latency stays well inside what a dependent's gate tolerates
/// however long the budget, and the probe stops once the node asserts.
#[cfg(all(
    feature = "liveness-monitor",
    feature = "coupling-observe",
    feature = "readiness"
))]
const READY_PROBE_DIVISOR: u64 = 8;

/// A `ready_on_write` node was handed off: wakes the monitor to re-plan, so the
/// node's readiness probe starts at its start rather than a budget later.
/// Only monitors wait here, one per graph: two slots cover two graphs, a third
/// graph's monitor waits out its own timer (`wait_past` is raced against it).
#[cfg(all(
    feature = "liveness-monitor",
    feature = "coupling-observe",
    feature = "readiness"
))]
pub(crate) static PROBE_EVT: WakerSet<2> = WakerSet::new();

/// The default `slot_timeout`: the bound on each pre-spawn gate (executor
/// slot, every resource gate, every `ready` dep), overridable per node, and
/// the window of the `ready_on_write` probe.
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
pub(crate) mod flag {
    /// Shutdown requested by the supervisor. Cleared by `reset()`.
    pub const SHUTDOWN: u16 = 1 << 0;
    /// The instance acked the shutdown (a flag, not a count, since every node
    /// is single-instance). Cleared by `reset()`. Set only by `settle_dropped`,
    /// in the same update that clears `RUNNING`: a node with `DROPPED` set is
    /// never `RUNNING`.
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
    /// Flipped against `NodeCfg::disabled_at_boot`: the node is disabled when
    /// exactly one of the two is set. Stored this way so a fresh handle is all
    /// zeros and lives in `.bss`. Lifecycle-spanning: not cleared by
    /// `reset()`, so a manual stop sticks against the automatic bring-up paths
    /// until `Supervisor::activate`; living in a `static`, it also survives a
    /// RAM-retaining power-state transition.
    pub const DISABLED_FLIP: u16 = 1 << 5;
    /// Self-managed: the supervisor never drives this node. Not cleared by
    /// `reset()`. Full rationale on `TaskNode::set_detached`.
    pub const DETACHED: u16 = 1 << 6;
    /// Task-asserted readiness ("initialized and serving") — distinct from
    /// `RUNNING` (spawned). Set by `set_ready()`, cleared by `clear_ready()`
    /// and by `reset()` so a respawned provider re-asserts.
    #[cfg(feature = "readiness")]
    pub const READY: u16 = 1 << 7;
    /// Stopped by a bound provider's `clear_ready` — or never started, when a
    /// bound dep was still un-ready at the end of the bring-up gate budget —
    /// and eligible to come back when readiness returns. Deliberately NOT
    /// disabled: a manual stop must survive a readiness flap, and a bound
    /// stop must not survive the provider's recovery. Not cleared by `reset()`
    /// — it spans the stopped instance's whole absence.
    #[cfg(feature = "bound-deps")]
    pub const BOUND_STOPPED: u16 = 1 << 8;
    /// `BOUND_STOPPED` where the feature is on, else nothing: the bits a handoff clears.
    #[cfg(feature = "bound-deps")]
    pub const HANDOFF_CLEARS: u16 = BOUND_STOPPED;
    #[cfg(not(feature = "bound-deps"))]
    pub const HANDOFF_CLEARS: u16 = 0;
    /// Stopped as a dependent of a deactivated node. `deactivate` marks only
    /// its seed as disabled; dependents are marked `COLLATERAL` instead.
    /// Both block automatic bring-up, but `activate` clears `COLLATERAL` once
    /// no disabled node remains in the node's transitive dependencies. A manual
    /// `start_node` also clears it. Unlike `BOUND_STOPPED`, a readiness flap
    /// does not release a control stop. Lifecycle-spanning: not cleared by `reset()`.
    pub const COLLATERAL: u16 = 1 << 9;
    /// `fault-inject`: swallowed ack pending replay.
    #[cfg(feature = "fault-inject")]
    pub const PENDING_ACK: u16 = 1 << 10;
    /// `fault-inject`: swallowed exit pending replay.
    #[cfg(feature = "fault-inject")]
    pub const PENDING_EXIT: u16 = 1 << 11;
    /// A resident `task:` shell is spawned for this node: it never returns,
    /// and the spawn glue restarts it in place instead of spawning again. Set
    /// once by the glue after the first spawn; never cleared.
    pub const RESIDENT: u16 = 1 << 12;
    /// The resident shell finished an activation (worker returned, resources
    /// restored, exit recorded) and is parked for the next start. Set by the
    /// shell as its last act; cleared by the glue's start. Not cleared by
    /// `reset()`.
    pub const IDLE: u16 = 1 << 13;
    /// `liveness-monitor`: this node's last health transition found the mailbox
    /// full. The next sweep sends it again, or drops it when the node has
    /// flipped back since; a stop clears it, and so does `reset()` at every
    /// activation.
    #[cfg(feature = "liveness-monitor")]
    pub const HEALTH_PENDING: u16 = 1 << 14;
}

/// Coordination state for one task: the live half of a [`TaskNode`], a
/// `static` of its own that the node points at.
///
/// Every node is single-instance, so the state is one word of per-node flags
/// plus single-consumer signals — no counts, no fan-out. Written by one side
/// (task or supervisor) and read by the other: the supervisor requests exit
/// (`SHUTDOWN` + `shutdown_wake`), the task acks it (`DROPPED`, announced on
/// the shared stop event), a parked Pause-mode task resumes on `resume_wake`, and
/// the scaling policy reads `RUNNING`/`BUSY`. See the private `flag` module
/// for each bit.
pub struct TaskHandle {
    /// The lifecycle flags, one bit each (see [`flag`]) — one atomic word
    /// where separate booleans would pad the handle. Writes pair `Release`
    /// with `Acquire` reads, per flag.
    flags: AtomicU16,
    /// Wake source for `wait_shutdown()`. Fired by `signal_shutdown()`.
    pub(crate) shutdown_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Wake source for `wait_resume()` on Pause-mode tasks. Fired by
    /// `signal_resume()`. Also the resident shell's start signal: the shell
    /// parks on it only after its worker returned, and a worker parks on it
    /// only while running, so the two waits never overlap. The shell re-checks
    /// `IDLE` after waking, so a stale latch cannot start it.
    pub(crate) resume_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Wake source for `wait_ready()`. Latching, single waiter: one app task
    /// may park here before the fill. The supervisor's bring-up never does; it
    /// watches the shared gate event.
    #[cfg(feature = "readiness")]
    ready_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Instant ticks (truncated) of the last granted beat; also stamped by
    /// `prepare_activation`, so a fresh activation is never instantly stale.
    #[cfg(feature = "liveness")]
    last_beat: AtomicU32,
    /// Activation generation, bumped once per activation by `mark_running`.
    /// Starts at 0 (never activated); the first spawn makes it 1. Wrapping:
    /// dependents compare for inequality, never ordering.
    #[cfg(feature = "epochs")]
    epoch: AtomicU32,
    /// Wake source for `wait_epoch_change()`. Single-waiter Signal, like
    /// `ready_wake`; the multi-consumer path is polling `epoch()`.
    #[cfg(feature = "epochs")]
    epoch_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Consecutive monitor sweeps that found this node stale. Reset to 0 by any
    /// sweep that finds it beating, stopped or detached, and by every
    /// activation (`prepare_activation`). Saturates at `beat_window`: once the
    /// event has been emitted the count only needs to hold the threshold, so a
    /// node stale for hours cannot wrap back around and re-report.
    #[cfg(feature = "liveness-monitor")]
    pub(crate) stale_strikes: AtomicU8,
    /// Wrapping sum of this node's beat-feeding `observed` write counters as
    /// of the last sweep. One word per node rather than one per entry, which
    /// is why a `beat` entry's token must be a counter — a documented
    /// requirement on [`Observer`], since value-tokens could cancel in the sum.
    #[cfg(all(feature = "coupling-observe", feature = "liveness-monitor"))]
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
    /// Encoded current fault.
    #[cfg(feature = "fault-inject")]
    pub(crate) fault: AtomicU8,
    /// Hog spin duration in milliseconds.
    #[cfg(feature = "fault-inject")]
    hog_ms: AtomicU32,
    /// Wakes the task on inject or clear.
    #[cfg(feature = "fault-inject")]
    pub(crate) fault_waker: embassy_sync::waitqueue::AtomicWaker,
    /// The executor task id currently running this node (`TaskRef::id()`, captured
    /// from the `SpawnToken` by the macro's spawn glue). `0` = unknown (not yet
    /// spawned, or a parked/closure-spawned node that never registered). Overwritten
    /// on every (re)spawn, so — unlike an external tracker — it stays correct across
    /// respawns without any unlinking.
    #[cfg(feature = "trace")]
    pub(crate) task_id: AtomicU32,
    /// Accumulated executor-poll time for this node, in embassy-time ticks,
    /// wrapping. Consumers sample twice and `wrapping_sub` to get a rate; the
    /// crate does no windowing.
    #[cfg(feature = "trace")]
    pub(crate) exec_ticks: AtomicU32,
    /// Number of executor polls of this node, wrapping.
    #[cfg(feature = "trace")]
    pub(crate) polls: AtomicU32,
    /// Longest single poll ever observed, in ticks — the "never yields" watermark.
    /// A large value names the node that hogged the executor even after the fact,
    /// which a live check cannot do from the blocked executor itself.
    #[cfg(feature = "trace")]
    pub(crate) max_poll_ticks: AtomicU32,
    /// The last [`CrumbPhase`] this node passed through, as its `repr(u8)`.
    /// Relaxed everywhere: a diagnostic, never a synchronisation point.
    #[cfg(feature = "breadcrumb")]
    pub(crate) crumb: AtomicU8,
    /// Explicit restarts of this node inside the current budget window.
    /// Written only by the supervisor task (`Supervisor::restart`).
    #[cfg(feature = "restart-intensity")]
    pub(crate) restarts: AtomicU16,
    /// Low half of the `Instant` ticks at which the current budget window
    /// opened. Full width, so a restart long after the window closed never
    /// aliases into it, as two halves so the handle keeps 4-byte alignment and
    /// no critical section is needed. Written only by the supervisor task
    /// (`Supervisor::restart`), so plain relaxed loads and stores; a torn read
    /// would need a second writer.
    #[cfg(feature = "restart-intensity")]
    pub(crate) restart_window_lo: AtomicU32,
    /// High half of the window start; see `restart_window_lo`.
    #[cfg(feature = "restart-intensity")]
    pub(crate) restart_window_hi: AtomicU32,
}

impl TaskHandle {
    /// A node's live state, all zeros, so a `static TaskHandle` lives in
    /// `.bss`. [`supervisor_graph!`] emits one per node and pool member.
    pub const fn new() -> Self {
        Self {
            flags: AtomicU16::new(0),
            shutdown_wake: Signal::new(),
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
            #[cfg(all(feature = "coupling-observe", feature = "liveness-monitor"))]
            write_mark: AtomicU32::new(0),
            #[cfg(feature = "liveness")]
            pending_beat: AtomicBool::new(false),
            #[cfg(feature = "node-status")]
            status: BlockingMutex::new(Cell::new(None)),
            #[cfg(feature = "fault-inject")]
            fault: AtomicU8::new(0),
            #[cfg(feature = "fault-inject")]
            hog_ms: AtomicU32::new(0),
            #[cfg(feature = "fault-inject")]
            fault_waker: embassy_sync::waitqueue::AtomicWaker::new(),
            #[cfg(feature = "trace")]
            task_id: AtomicU32::new(0),
            #[cfg(feature = "trace")]
            exec_ticks: AtomicU32::new(0),
            #[cfg(feature = "trace")]
            polls: AtomicU32::new(0),
            #[cfg(feature = "trace")]
            max_poll_ticks: AtomicU32::new(0),
            #[cfg(feature = "breadcrumb")]
            crumb: AtomicU8::new(CrumbPhase::Idle as u8),
            #[cfg(feature = "restart-intensity")]
            restarts: AtomicU16::new(0),
            #[cfg(feature = "restart-intensity")]
            restart_window_lo: AtomicU32::new(0),
            #[cfg(feature = "restart-intensity")]
            restart_window_hi: AtomicU32::new(0),
        }
    }

    pub(crate) fn flag(&self, bit: u16) -> bool {
        self.flags.load(Ordering::Acquire) & bit != 0
    }

    pub(crate) fn flag_set(&self, bit: u16) {
        self.flags.fetch_or(bit, Ordering::Release);
    }

    /// Clear `bits` — one bit or a whole mask, as `reset()` uses it.
    pub(crate) fn flag_clear(&self, bits: u16) {
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
    pub(crate) fn flag_swap(&self, bit: u16, on: bool) -> bool {
        let prior = if on {
            self.flags.fetch_or(bit, Ordering::Release)
        } else {
            self.flags.fetch_and(!bit, Ordering::Release)
        };
        prior & bit != 0
    }
}

impl Default for TaskHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a truncated-tick age saturates instead of wrapping: half the `u32`
/// tick range, about 35.8 min at a 1 MHz tick. The node heartbeat's.
#[cfg(feature = "liveness")]
pub(crate) const AGE_CAP: u32 = u32::MAX / 2;

/// Age of a truncated-tick `stamp`, saturating at [`AGE_CAP`]: past it the
/// stamp is re-anchored to read exactly `AGE_CAP`, so the age never wraps
/// back to small. The stamp is loaded before the clock is read, so the one
/// used is never newer than `now`; one stored in between is newer still, and
/// the compare-exchange leaves it in place.
#[cfg(feature = "liveness")]
pub(crate) fn saturating_age(stamp: &portable_atomic::AtomicU32) -> u32 {
    let last = stamp.load(Ordering::Acquire);
    let now = embassy_time::Instant::now().as_ticks() as u32;
    let age = now.wrapping_sub(last);
    if age <= AGE_CAP {
        return age;
    }
    let _ = stamp.compare_exchange(
        last,
        now.wrapping_sub(AGE_CAP),
        Ordering::AcqRel,
        Ordering::Relaxed,
    );
    AGE_CAP
}

/// `max_age` in ticks, clamped just below [`AGE_CAP`] so a saturated age
/// still exceeds it.
#[cfg(feature = "liveness")]
pub(crate) fn age_bound(max_age: embassy_time::Duration) -> u64 {
    max_age.as_ticks().min(u64::from(AGE_CAP) - 1)
}

// ─── TaskNode ────────────────────────────────────────────────────────────

/// A node in the supervisor's task graph.
///
/// Designed to live in `static` memory: every field is `Sync`, all constructors
/// are `const`. [`supervisor_graph!`] emits one per managed task, along with
/// the [`Graph`] (`GRAPH`) that [`Supervisor::new`] consumes.
///
/// Two pointers, so the node static itself is flash: the [`NodeCfg`] (name,
/// mode, spawn fn, gates, budgets, tables) has no interior mutability and
/// stays in flash; the [`TaskHandle`] (flags, wakes, counters) is all zeros at
/// rest and sits in `.bss`, costing RAM only. Declared by
/// [`supervisor_graph!`] as three statics per node.
///
/// Built by hand, a node is the same three statics:
///
/// ```
/// use embassy_supervisor::{Mode, NodeCfg, TaskHandle, TaskNode};
///
/// static CFG: NodeCfg = NodeCfg::new("n", Mode::Terminate, None);
/// static H: TaskHandle = TaskHandle::new();
/// static N: TaskNode = TaskNode::new(&CFG, &H);
///
/// assert_eq!(N.name(), "n");
/// ```
pub struct TaskNode {
    /// The immutable half, flash-resident, emitted as its own `static` by
    /// [`supervisor_graph!`] beside the node.
    pub(crate) cfg: &'static NodeCfg,
    /// The live half, a zeroed `.bss` `static` emitted beside it.
    pub(crate) handle: &'static TaskHandle,
}

/// The immutable half of a [`TaskNode`]: everything the graph *declared* about
/// the node, none of what happens to it at runtime. No interior mutability, so
/// the `static` carrying it lives in flash (`.rodata`), as does the node that
/// points at it. Built `const` with [`new`](Self::new) plus the chainable
/// `with_*` methods, exactly as [`supervisor_graph!`] emits it.
pub struct NodeCfg {
    /// Human-readable name. Used in defmt logs and panic messages.
    name: &'static str,
    /// Lifecycle policy. See [`Mode`].
    mode: Mode,
    /// App-provided spawn function (typically an inline closure at the node's
    /// declaration). Called once at boot from `Supervisor::start`, again from
    /// `respawn_terminate` for Terminate nodes, and at runtime from `start_node`
    /// for `OnDemand` nodes. `None` for a **parked** node the application spawns
    /// itself (e.g. a `Pause` sensor holding a peripheral handle): the supervisor
    /// tracks its lifecycle but never spawns it.
    pub(crate) spawn: Option<fn(Spawner) -> Result<(), SpawnError>>,
    /// The executor [`SpawnerSlot`] this node spawns through (`executor: NAME` in
    /// the graph), or `None` to spawn on the supervisor's own `Spawner`. When
    /// `Some`, the supervisor holds the spawn until the slot is filled
    /// (bounded by [`slot_timeout`](TaskNode::slot_timeout)), so the
    /// generated glue's own non-blocking `SpawnerSlot::get` is already filled. Set
    /// by the macro via [`with_executor`](Self::with_executor); `const`, zero-cost.
    pub(crate) spawn_slot: Option<&'static SpawnerSlot>,
    /// The [`ResourceSlot`]s this node's spawn takes from (`resources:` in the
    /// graph), type-erased to their [`ResourceGate`] readiness view. The
    /// supervisor holds the spawn until every gate is filled (bounded by
    /// [`slot_timeout`](TaskNode::slot_timeout)), so (a) a `main` that
    /// provides late is tolerated and (b) a respawn cannot race the previous
    /// instance's shell restoring the value (the restore happens after the
    /// worker's shutdown ack). Empty for nodes without `resources:`. Set by the
    /// macro via [`with_resources`](Self::with_resources); `const`, zero-cost.
    pub(crate) resource_gates: &'static [&'static dyn ResourceGate],
    /// The slots this node's task fills at runtime (`provides:` in the graph),
    /// cleared when the node acknowledges a stop so a consumer's gate wait
    /// sees the value's absence rather than a previous activation's leftover.
    /// A `Pause` park is exempt (the parked task still backs what it
    /// published); an exit is not.
    provides: &'static [&'static dyn ResourceGate],
    /// `divisible` budget slots this node claims (`resources:`). Released on
    /// shutdown, or by the supervisor if the ack times out, so a dead holder
    /// does not strand its share. A `Pause` park keeps the claim; an exit
    /// releases it.
    #[cfg(feature = "budget")]
    claims: &'static [(&'static dyn Divisible, u8)],
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
    /// Whole milliseconds, rounded up: no field of the config is a `Duration`,
    /// so it aligns to 4 on a 32-bit target.
    pub(crate) slot_timeout_ms: u32,
    /// How long a stop waits for this node's shutdown ack before faulting it
    /// (`ack_timeout:` in the graph). Defaults to [`SHUTDOWN_ACK_TIMEOUT_MS`]
    /// (2 s); raise it for a node whose cleanup legitimately takes longer —
    /// a flash sync, a peripheral settle. Set by the macro via
    /// [`with_ack_timeout`](Self::with_ack_timeout). Whole milliseconds,
    /// rounded up.
    pub(crate) ack_timeout_ms: u32,
    /// How long this node may go without a `beat()` before the monitor calls it
    /// stale (`beat_timeout:` in the graph), in ticks. `u32::MAX`, the default,
    /// opts the node OUT of policing entirely, which is right for every node
    /// whose body does not beat: it would otherwise read permanently stale.
    /// [`with_beat_timeout`](Self::with_beat_timeout) refuses any value at or
    /// past `u32::MAX / 2`, so the marker never collides with a budget.
    #[cfg(feature = "liveness-monitor")]
    beat_ticks: u32,
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
    /// `fault-inject`: true if the node uses a `task:` shell wrapped by [`Injected`].
    #[cfg(feature = "fault-inject")]
    shelled: bool,
    /// `disabled` in the graph: the node starts down and waits for a control
    /// `Activate` (or an explicit `start_node`).
    disabled_at_boot: bool,
    /// `enabled_if:` in the graph: a predicate [`Supervisor::start`] evaluates
    /// once per whole-graph start; `false` latches the node `disabled`.
    #[cfg(feature = "enabled-if")]
    pub(crate) enabled_if: Option<fn() -> bool>,
    /// `restart_budget: MAX in WINDOW_MS` in the graph: at most `MAX` explicit
    /// [`Supervisor::restart`]s of this node per window. `0`: no budget, the
    /// restarts are unlimited.
    #[cfg(feature = "restart-intensity")]
    restart_max: u16,
    /// The `restart_budget:` window, in whole milliseconds rounded up.
    #[cfg(feature = "restart-intensity")]
    restart_window_ms: u32,
    /// `metadata-names`: [`name`](Self::name) plus a trailing NUL, the string
    /// stamped into the task's executor metadata (SystemView reads task names
    /// as C strings). `None` keeps the executor's default name.
    #[cfg(feature = "metadata-names")]
    trace_name: Option<&'static str>,
}

/// `d` in whole milliseconds, rounded up, as the `u32` a [`NodeCfg`] stores.
/// Refuses a value past `u32::MAX` milliseconds (about 49.7 days), at compile
/// time when the config is a `static`.
const fn ms_ceil_u32(d: embassy_time::Duration, what: &str) -> u32 {
    let ms = ceil_ms(d);
    assert!(ms <= u32::MAX as u64, "{}", what);
    ms as u32
}

impl NodeCfg {
    /// The declared side of a single-instance node started at boot
    /// (`Terminate`/`Pause`) or on demand (`Mode::OnDemand`). Every node is
    /// single-instance; an elastic service is modelled as a `pool` with `task:`
    /// members: their resident shells are restarted in place, never spawned
    /// while a previous activation is still finishing.
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
            #[cfg(feature = "budget")]
            claims: &[],
            #[cfg(feature = "readiness")]
            ready_deps: &[],
            slot_timeout_ms: SLOT_READY_TIMEOUT.as_millis() as u32,
            ack_timeout_ms: SHUTDOWN_ACK_TIMEOUT_MS as u32,
            #[cfg(feature = "liveness-monitor")]
            beat_ticks: u32::MAX,
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
            #[cfg(feature = "fault-inject")]
            shelled: false,
            disabled_at_boot: false,
            #[cfg(feature = "enabled-if")]
            enabled_if: None,
            #[cfg(feature = "restart-intensity")]
            restart_max: 0,
            #[cfg(feature = "restart-intensity")]
            restart_window_ms: 0,
            #[cfg(feature = "metadata-names")]
            trace_name: None,
        }
    }

    /// Declare the node down at boot (the `disabled` graph marker): bring-up
    /// skips it until a control `Activate` or an explicit `start_node`, and
    /// [`TaskNode::is_disabled`] reads `true` from the start. `const` and
    /// chainable in a `static` initializer; emitted by [`supervisor_graph!`].
    pub const fn with_disabled_at_boot(mut self, disabled: bool) -> Self {
        self.disabled_at_boot = disabled;
        self
    }

    /// Gate this node's whole-graph bring-up on `enabled` (`enabled_if:` in
    /// the graph). [`Supervisor::start`] calls it once per call, before the
    /// wave, for a node that is down (running and detached nodes are left
    /// alone); a
    /// `false` latches the node `disabled`, exactly as the graph's `disabled`
    /// marker would — dashboards see it, and `activate`/`start_node` still
    /// override it. A `true` never clears a latch (a manual `deactivate`
    /// survives), and `respawn_terminate` and the control verbs do not
    /// re-evaluate it. `const` and chainable in a `static` initializer;
    /// emitted by [`supervisor_graph!`].
    #[cfg(feature = "enabled-if")]
    pub const fn with_enabled_if(mut self, enabled: fn() -> bool) -> Self {
        self.enabled_if = Some(enabled);
        self
    }

    /// Allow at most `max` explicit [`Supervisor::restart`]s of this node
    /// within `window` (`restart_budget: MAX in WINDOW_MS` in the graph); the
    /// next one inside the window fails with [`FaultKind::RestartStorm`]
    /// without stopping anything. `max` 0 is treated as 1. The window is kept
    /// in whole milliseconds, rounded up; one past `u32::MAX` milliseconds is
    /// refused at compile time in a `static`. `const` and chainable in a
    /// `static` initializer; emitted by [`supervisor_graph!`].
    #[cfg(feature = "restart-intensity")]
    pub const fn with_restart_budget(mut self, max: u16, window: embassy_time::Duration) -> Self {
        self.restart_max = if max == 0 { 1 } else { max };
        self.restart_window_ms =
            ms_ceil_u32(window, "restart_budget window exceeds u32 milliseconds");
        self
    }

    /// The NUL-terminated name `metadata-names` stamps into the task's executor
    /// metadata. Also re-points [`name`](TaskNode::name) at the same bytes, so
    /// the graph carries one literal per node, not two. `const` and chainable in
    /// a `static` initializer; emitted by [`supervisor_graph!`]. Panics (at
    /// compile time in a `static`) unless `name` ends with a NUL.
    #[cfg(feature = "metadata-names")]
    pub const fn with_trace_name(mut self, name: &'static str) -> Self {
        let b = name.as_bytes();
        assert!(
            !b.is_empty() && b[b.len() - 1] == 0,
            "a trace name ends with a NUL"
        );
        self.name = name.split_at(name.len() - 1).0;
        self.trace_name = Some(name);
        self
    }

    /// Mark the node as a `task:` shell. Only shells can be stalled, crashed or hogged.
    #[cfg(feature = "fault-inject")]
    pub const fn with_shell(mut self) -> Self {
        self.shelled = true;
        self
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
    /// graph clause); a stop ack or an exit clears them (a `Pause` park keeps
    /// them). `const` and chainable in a `static` initializer; emitted by
    /// [`supervisor_graph!`].
    pub const fn with_provides(mut self, slots: &'static [&'static dyn ResourceGate]) -> Self {
        self.provides = slots;
        self
    }

    /// Declare the `divisible` budget slots this node claims (`resources:`).
    /// Released on stop. `const` and chainable; emitted by [`supervisor_graph!`].
    #[cfg(feature = "budget")]
    pub const fn with_claims(mut self, claims: &'static [(&'static dyn Divisible, u8)]) -> Self {
        self.claims = claims;
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
    /// `NodeFault`, just later). Kept in whole milliseconds, rounded up; a
    /// bound past `u32::MAX` milliseconds is refused at compile time in a
    /// `static`. `const` and chainable in a `static` initializer; emitted by
    /// [`supervisor_graph!`].
    pub const fn with_slot_timeout(mut self, timeout: embassy_time::Duration) -> Self {
        self.slot_timeout_ms = ms_ceil_u32(timeout, "slot_timeout exceeds u32 milliseconds");
        self
    }

    /// Override how long a stop waits for this node's shutdown ack before
    /// faulting it with [`FaultKind::ShutdownTimeout`] (the
    /// `ack_timeout: <millis>` graph clause, default 2 s). Raise it for a node
    /// whose cleanup legitimately outlasts the default — a flash sync, a
    /// peripheral settle; the missed-ack failure mode stays a loud
    /// [`NodeFault`], just later. Kept in whole milliseconds, rounded up; a
    /// timeout past `u32::MAX` milliseconds is refused at compile time in a
    /// `static`. `const` and chainable in a `static` initializer; emitted by
    /// [`supervisor_graph!`].
    pub const fn with_ack_timeout(mut self, timeout: embassy_time::Duration) -> Self {
        self.ack_timeout_ms = ms_ceil_u32(timeout, "ack_timeout exceeds u32 milliseconds");
        self
    }

    /// Opt this node into liveness policing (the `beat_timeout: <millis>` graph
    /// clause): [`Supervisor::monitor`] reports it once it has been running
    /// without a [`beat`](TaskNode::beat) for longer than `timeout`.
    ///
    /// A beat is credited when a check next looks, not when it happens, so a
    /// stall is reported between one and two `timeout`s after the last beat.
    /// A beat's age saturates at `u32::MAX / 2` ticks (about 35.8 min at a
    /// 1 MHz tick), and a `timeout` at or past that is refused at compile time
    /// in a `static` initializer (the age could never exceed it).
    ///
    /// Only declare this on a node whose body actually beats: an un-beating
    /// node reads permanently stale. `const` and chainable in a `static`
    /// initializer; emitted by [`supervisor_graph!`].
    #[cfg(feature = "liveness-monitor")]
    pub const fn with_beat_timeout(mut self, timeout: embassy_time::Duration) -> Self {
        assert!(
            timeout.as_ticks() < AGE_CAP as u64,
            "beat_timeout must be below u32::MAX / 2 ticks (about 35.8 min at 1 MHz)"
        );
        self.beat_ticks = timeout.as_ticks() as u32;
        self
    }

    /// How many consecutive stale sweeps the monitor requires before it reports
    /// this node (the `beat_window: <n>` graph clause, default 1). Raise it for
    /// a node whose beat interval is legitimately jittery: the effective
    /// grace period becomes roughly `beat_timeout` + `n` sweep periods. A sweep
    /// runs whenever any policed node in the graph is due, so a node beside a
    /// faster-budget one gets shorter sweep periods and fills its window sooner.
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
    /// application's decision. Without `liveness-monitor` nothing sweeps, so
    /// the flag has no effect.
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
    /// A node over its flash-resident [`NodeCfg`] and its [`TaskHandle`]. A
    /// control-started node (e.g. an OTA task) is declared down in its config
    /// ([`NodeCfg::with_disabled_at_boot`]); the handle starts all zeros.
    /// Each handle backs exactly one node. `const`; [`supervisor_graph!`]
    /// emits the config, the handle and this call together.
    pub const fn new(cfg: &'static NodeCfg, handle: &'static TaskHandle) -> Self {
        Self { cfg, handle }
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
        embassy_time::Duration::from_millis(self.cfg.slot_timeout_ms as u64)
    }

    /// How long a stop waits for this node's shutdown ack before faulting it
    /// (see [`with_ack_timeout`](NodeCfg::with_ack_timeout)). Both stop paths
    /// honor it: the single-node wait, and the whole-graph wave, where each
    /// node's window runs from the moment *it* is signalled.
    pub const fn ack_timeout(&self) -> embassy_time::Duration {
        embassy_time::Duration::from_millis(self.cfg.ack_timeout_ms as u64)
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
        match self.cfg.beat_ticks {
            u32::MAX => None,
            ticks => Some(embassy_time::Duration::from_ticks(ticks as u64)),
        }
    }

    /// Consecutive stale sweeps required before the monitor reports this node
    /// (see [`with_beat_window`](NodeCfg::with_beat_window)).
    #[cfg(feature = "liveness-monitor")]
    pub const fn beat_window(&self) -> u8 {
        self.cfg.beat_window
    }

    /// The node's `restart_budget:` as `(max, window)`, or `None` when its
    /// restarts are unlimited. See [`NodeCfg::with_restart_budget`].
    #[cfg(feature = "restart-intensity")]
    pub const fn restart_budget(&self) -> Option<(u16, embassy_time::Duration)> {
        match self.cfg.restart_max {
            0 => None,
            max => Some((
                max,
                embassy_time::Duration::from_millis(self.cfg.restart_window_ms as u64),
            )),
        }
    }

    /// The last lifecycle phase a supervisor drove this node through —
    /// [`CrumbPhase::Idle`] if none has yet. Relaxed: a diagnostic snapshot,
    /// not a synchronisation point. See [`crumb_word`] for the postmortem use.
    #[cfg(feature = "breadcrumb")]
    pub fn crumb(&self) -> CrumbPhase {
        CrumbPhase::from_u8(self.handle.crumb.load(Ordering::Relaxed)).unwrap_or(CrumbPhase::Idle)
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
    pub(crate) fn entries(&self, is_write: bool) -> impl Iterator<Item = &'static Coupling> {
        let tables = if is_write {
            self.cfg.writes
        } else {
            self.cfg.reads
        };
        tables.iter().flat_map(|t| t.iter())
    }

    /// Is `signal` among this node's entries in the given direction?
    #[cfg(feature = "coupling")]
    pub(crate) fn has_entry(&self, signal: &Coupling, is_write: bool) -> bool {
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
    // Called from inside the task body. The task-side protocol is three rules
    // (the guide chapter "Writing supervised tasks"): race every blocking await
    // against shutdown, call `mark_exited()` on an autonomous exit, and let
    // resources follow the mode.

    /// True if the supervisor has asked this node to shut down.
    /// A [`Fault::Wedge`] hides the request until cleared.
    pub fn shutdown_requested(&self) -> bool {
        #[cfg(feature = "fault-inject")]
        if self.fault() == Fault::Wedge {
            return false;
        }
        self.handle.flag(flag::SHUTDOWN)
    }

    /// Wait until the supervisor asks this node to shut down.
    /// A [`Fault::Wedge`] keeps this pending until cleared.
    ///
    /// Single waiter: the node's own task. The wake is a one-waker `Signal`, so
    /// a second task waiting on the same node evicts the first on every poll.
    pub async fn wait_shutdown(&self) {
        core::future::poll_fn(|cx| {
            if self.poll_shutdown(cx) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }

    /// Poll the shutdown request, used by the cancel/pause drivers.
    /// A [`Fault::Wedge`] hides the request but leaves the waker registered.
    pub(crate) fn poll_shutdown(&self, cx: &mut Context<'_>) -> bool {
        #[cfg(feature = "fault-inject")]
        self.handle.fault_waker.register(cx.waker());
        #[cfg(feature = "fault-inject")]
        if self.fault() == Fault::Wedge {
            return false;
        }
        self.handle.flag(flag::SHUTDOWN)
            || core::pin::pin!(self.handle.shutdown_wake.wait())
                .poll(cx)
                .is_ready()
    }

    /// Acknowledge that this node's instance has dropped and notify waiters.
    /// A [`Fault::Wedge`] swallows the ack until cleared.
    /// On a `Pause` node this is the park: its provided slots and claims stay.
    pub fn ack_dropped(&self) {
        self.release_provided(true);
        #[cfg(feature = "fault-inject")]
        if self.wedge_swallows(flag::PENDING_ACK) {
            return;
        }
        self.settle_dropped();
    }

    /// Swallow an ack while wedged. Returns true if the clear must deliver it.
    #[cfg(feature = "fault-inject")]
    fn wedge_swallows(&self, bits: u16) -> bool {
        if self.fault() == Fault::Wedge {
            self.handle.flag_set(bits);
            if self.fault() == Fault::Wedge {
                return true;
            }
            let prev = self.handle.flags.fetch_and(!bits, Ordering::AcqRel);
            if prev & flag::PENDING_ACK == 0 {
                return true;
            }
        }
        false
    }

    /// Drop what this instance provided and release its claims, unless the ack
    /// is a `Pause` node's park (`park` is true only from `ack_dropped`): a
    /// parked task still backs what it published, an exited one does not.
    fn release_provided(&self, park: bool) {
        if park && matches!(self.cfg.mode, Mode::Pause) {
            return;
        }
        for gate in self.cfg.provides {
            gate.clear();
        }
        #[cfg(feature = "budget")]
        self.release_claims();
    }

    /// Update flags and wakes to record the dropped instance. RUNNING clears and
    /// DROPPED sets in one update, so a `mark_running` racing this ack from the
    /// other core either sees DROPPED or has its RUNNING cleared here.
    fn settle_dropped(&self) {
        let prior = self
            .handle
            .flags
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |f| {
                Some((f & !flag::RUNNING) | flag::DROPPED)
            })
            .unwrap_or_else(|f| f);
        STOP_EVT.signal();
        // A member that stops unasked changes its pool's count behind the pool
        // driver's back, so the driver re-evaluates. An asked-for stop belongs
        // to the wave that asked: waking the driver in the middle of that wave
        // could regrow the member before the wave has seen it go.
        if prior & flag::SHUTDOWN == 0 {
            request_scale();
        }
        #[cfg(feature = "bound-deps")]
        notify_bind();
        #[cfg(all(feature = "data-deps", feature = "readiness"))]
        crate::data_deps::notify_serving();
    }

    /// Deliver an ack or exit that a wedge swallowed.
    #[cfg(feature = "fault-inject")]
    fn replay_swallowed(&self) {
        let pending = self
            .handle
            .flags
            .fetch_and(!(flag::PENDING_ACK | flag::PENDING_EXIT), Ordering::AcqRel);
        if pending & flag::PENDING_ACK != 0 {
            if pending & flag::PENDING_EXIT != 0 {
                self.handle.flag_set(flag::COMPLETED);
            }
            self.settle_dropped();
        }
    }

    /// Give back every `divisible` share this node holds (see
    /// [`NodeCfg::with_claims`]). Idempotent: releasing an empty slot stores zero.
    #[cfg(feature = "budget")]
    pub fn release_claims(&self) {
        for (budget, slot) in self.cfg.claims {
            budget.release(*slot);
        }
    }

    /// Mark the task as completed and acknowledge its drop.
    /// A wedge hides this until cleared. Clears provided slots and releases
    /// claims in every mode: an exit is never a park.
    pub fn mark_exited(&self) {
        self.release_provided(false);
        #[cfg(feature = "fault-inject")]
        if self.wedge_swallows(flag::PENDING_EXIT | flag::PENDING_ACK) {
            return;
        }
        self.handle.flag_set(flag::COMPLETED);
        self.settle_dropped();
    }

    /// Current injected fault, or `None` if healthy.
    #[cfg(feature = "fault-inject")]
    pub fn fault(&self) -> Fault {
        Fault::decode(
            self.handle.fault.load(Ordering::Acquire),
            self.handle.hog_ms.load(Ordering::Relaxed),
        )
    }

    /// Inject a fault, or clear it with `Fault::None`.
    /// Stall, crash and hog need a `task:` shell; wedge works on any node.
    #[cfg(feature = "fault-inject")]
    pub fn inject(&self, fault: Fault) -> Result<(), InjectError> {
        if fault == Fault::None {
            self.clear_fault();
            return Ok(());
        }
        if fault != Fault::Wedge && !self.cfg.shelled {
            return Err(InjectError::NoShell);
        }
        let (code, ms) = fault.encode();
        self.handle.hog_ms.store(ms, Ordering::Relaxed);
        self.handle.fault.store(code, Ordering::Release);
        // Replacing a wedge releases anything it was holding back.
        if fault != Fault::Wedge {
            self.replay_swallowed();
        }
        // Wake the shell so stall or hog take effect immediately.
        self.handle.fault_waker.wake();
        Ok(())
    }

    /// Clear the injected fault and replay anything it withheld.
    #[cfg(feature = "fault-inject")]
    pub fn clear_fault(&self) {
        self.handle.fault.swap(0, Ordering::AcqRel);
        self.replay_swallowed();
        // Wake any worker parked on the now-visible shutdown request.
        // Do not re-fire `shutdown_wake`; its latch would outlive this fault.
        self.handle.fault_waker.wake();
    }

    /// Macro glue: a shell found a gated slot empty at activation.
    #[doc(hidden)]
    pub fn __sv_lost_resource(&self) {
        warn!(
            "supervisor: {} lost a resource between its gate check and its activation",
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
    ///
    /// Idempotent: a call that does not change the flag wakes nobody.
    pub fn set_ready(&self) {
        if self.handle.flag_swap(flag::READY, true) {
            return;
        }
        self.handle.ready_wake.signal(());
        __sv_gate_event();
        #[cfg(feature = "data-deps")]
        crate::data_deps::notify_serving();
        #[cfg(feature = "bound-deps")]
        notify_bind();
        #[cfg(feature = "pool")]
        request_scale();
    }

    #[cfg(feature = "readiness")]
    /// Clear this node's readiness.
    ///
    /// Idempotent: a call that does not change the flag wakes nobody.
    pub fn clear_ready(&self) {
        if self.handle.flag_swap(flag::READY, false) {
            #[cfg(feature = "bound-deps")]
            notify_bind();
        }
    }

    #[cfg(feature = "readiness")]
    /// Return whether this node is currently ready.
    pub fn is_ready(&self) -> bool {
        self.handle.flag(flag::READY)
    }

    #[cfg(feature = "readiness")]
    /// Wait until this node becomes ready.
    ///
    /// Single waiter: the wake-up is a latching `Signal` with one waker slot.
    /// The supervisor's bring-up does not wait here; it watches the shared
    /// gate event, so the slot is the app's. Two concurrent waiters evict each
    /// other's waker on every poll, and after a `set_ready` only the first to
    /// poll returns. Fan out through an app-owned `embassy_sync::watch::Watch`,
    /// or poll [`is_ready`](Self::is_ready).
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

    /// The wrapping sum of every `observed beat` write entry's change count,
    /// or `None` when the node declares no such entry.
    #[cfg(all(feature = "coupling-observe", feature = "liveness-monitor"))]
    fn observed_mark(&self) -> Option<u32> {
        let mut mark = None;
        for w in self.entries(true) {
            if w.beats()
                && let Some(o) = w.observer()
            {
                mark = Some(mark.unwrap_or(0u32).wrapping_add(o.count()));
            }
        }
        mark
    }

    /// The monitor's change detection: did any `observed beat` entry advance
    /// since the previous call? Swaps the stored mark, so it consumes the
    /// advance; only the sweep calls it.
    #[cfg(all(feature = "coupling-observe", feature = "liveness-monitor"))]
    pub(crate) fn poll_observed_writes(&self) -> bool {
        self.observed_mark()
            .is_some_and(|mark| self.handle.write_mark.swap(mark, Ordering::AcqRel) != mark)
    }

    #[cfg(all(feature = "coupling-observe", feature = "liveness-monitor"))]
    fn seed_write_mark(&self) {
        self.handle
            .write_mark
            .store(self.observed_mark().unwrap_or(0), Ordering::Release);
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

    /// Ticks since the last [`beat`](Self::beat). A pending beat (`beat()`, or a
    /// `beat_put`/`beat_writer` write under `dataflow`) is granted by this
    /// call and reads as 0.
    ///
    /// The age saturates at `u32::MAX / 2` ticks (about 35.8 min at a 1 MHz
    /// tick) instead of wrapping, so a stalled node stays stale. That needs a
    /// check at least that often, which the monitor's sweep and a watchdog
    /// loop on `Graph::all_fresh` both provide.
    #[cfg(feature = "liveness")]
    pub fn ticks_since_beat(&self) -> u32 {
        // Granted with the clock read this check makes anyway, so the beating
        // task never reads the clock. Whoever sees it pending reports 0.
        if self.handle.pending_beat.load(Ordering::Relaxed) {
            let now = embassy_time::Instant::now().as_ticks() as u32;
            // Stamp before clearing: a concurrent checker sees the pending flag or
            // the new stamp, never neither.
            self.handle.last_beat.store(now, Ordering::Release);
            self.handle.pending_beat.store(false, Ordering::Release);
            return 0;
        }
        saturating_age(&self.handle.last_beat)
    }

    /// Ticks until this node next needs looking at, for the monitor's sleep.
    ///
    /// `None` when the node is unpoliced. Three cases, in the order they matter:
    ///
    /// * **Waiting to assert readiness** (`ready_on_write`, not yet ready): a
    ///   short probe for the activation's first `SLOT_READY_TIMEOUT`, then an
    ///   eighth of its budget. Readiness gates dependents' spawns against their
    ///   `slot_timeout`, so noticing the first write late spends someone else's
    ///   budget. Bring-up is brief and latency-sensitive; a fraction of the beat
    ///   budget, capped at a fraction of the default `slot_timeout`, buys that
    ///   back and costs nothing once the node is ready. The cap lifts once the
    ///   default gate budget is spent, so a node that never writes cannot hold
    ///   every other node's sweep at the capped rate.
    /// * **Overdue**: half a budget, so a stalled node's `beat_window` strikes
    ///   accumulate at a bounded rate instead of spinning on a zero delay.
    /// * **Running normally**: the first tick at which it is stale, one past
    ///   the budget.
    ///
    /// A node that is down or detached is re-examined a budget later: there is
    /// nothing to report about it now, but it may be running by then. A
    /// `ready_on_write` node that starts wakes the monitor early instead.
    #[cfg(feature = "liveness-monitor")]
    pub(crate) fn ticks_until_check(&self) -> Option<u64> {
        let budget = match self.cfg.beat_ticks {
            u32::MAX => return None,
            t => u64::from(t),
        };
        if self.is_detached() || !self.is_running() {
            return Some(budget);
        }
        // The probe serves the sweep-driven (`observed`) form only; a verb
        // write asserts readiness inline, with nothing to poll for.
        #[cfg(all(feature = "coupling-observe", feature = "readiness"))]
        if self.cfg.ready_on_write && !self.is_ready() {
            // `last_beat` holds the activation stamp until the first beat,
            // which here is the first observed write unless the body also
            // calls `beat()`, so this is the activation's age. Young: probe at
            // the capped rate so a dependent's gate sees the first write
            // quickly. Older than the default gate budget: an eighth of the
            // beat budget, so a node that never asserts cannot keep every
            // other node's sweep at 12.5 ms.
            let probe = budget / READY_PROBE_DIVISOR;
            let young = u64::from(self.ticks_since_beat()) < SLOT_READY_TIMEOUT.as_ticks();
            let probe = if young {
                probe.min(SLOT_READY_TIMEOUT.as_ticks() / READY_PROBE_DIVISOR)
            } else {
                probe
            };
            return Some(probe.max(1));
        }
        Some(
            // Stale means older than `budget`: the first stale tick is `budget + 1`.
            match (budget + 1).saturating_sub(u64::from(self.ticks_since_beat())) {
                0 => (budget / 2).max(1),
                remaining => remaining,
            },
        )
    }

    /// True when the node is running but hasn't beaten within `max_age`: the
    /// alive-but-stalled detector (a task hogging nothing, parked on an await
    /// that will never complete). Not-running nodes are never stale: a stopped
    /// or completed node is *down*, which `is_running`/`has_exited` already
    /// report. Complements the `trace` stall watermark, which catches the
    /// opposite failure (a poll that never yields). A `max_age` at or past
    /// `u32::MAX / 2` ticks is clamped just below it, where the age saturates.
    #[cfg(feature = "liveness")]
    pub fn is_stale(&self, max_age: embassy_time::Duration) -> bool {
        self.is_running() && u64::from(self.ticks_since_beat()) > age_bound(max_age)
    }

    /// This node's activation generation: `0` before the first spawn, then
    /// incremented once per activation the supervisor starts (a fresh spawn, a
    /// pool grow, a `respawn_terminate`, a `Pause` node's resume). The bump
    /// follows the handoff, so a provider on a higher-priority executor or
    /// another core can publish once before its new epoch is visible.
    ///
    /// **The dependent-side answer to "my provider was restarted underneath
    /// me".** `deps:` gates a spawn once; nothing re-gates a node that is
    /// *already running* when one of its providers cycles. A consumer holding
    /// derived state (a filter, a session, a cached handle) samples this once
    /// and compares it each iteration: one acquire load, cheap enough for a
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
    /// Wait until the node's epoch differs from `seen`, and return it.
    ///
    /// Single waiter, like [`wait_ready`](Self::wait_ready): two concurrent
    /// waiters evict each other's waker on every poll, and after a bump only
    /// the first to poll returns. Further dependents compare
    /// [`epoch`](Self::epoch) in their own loop.
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
    ///
    /// Single waiter: the node's own task. The wake is a one-waker `Signal`, so
    /// a second task waiting on the same node evicts the first on every poll.
    pub async fn wait_resume(&self) {
        self.handle.resume_wake.wait().await;
    }

    /// Run `fut` until it completes or the node is stopped, returning [`Aborted`] on stop.
    ///
    /// Single waiter: the node's own task. The wake is a one-waker `Signal`, so
    /// a second task waiting on the same node evicts the first on every poll.
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
    ///
    /// Single waiter: the node's own task. The wake is a one-waker `Signal`, so
    /// a second task waiting on the same node evicts the first on every poll.
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

    /// Run `fut` until it completes or the node is paused. A pause drops `fut`,
    /// acks and parks inside this call, which returns `Err(`[`Resumed`]`)` once
    /// the supervisor resumes the node.
    ///
    /// Single waiter: the node's own task. The wake is a one-waker `Signal`, so
    /// a second task waiting on the same node evicts the first on every poll.
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

    /// Return `true` if the node is disabled: declared `disabled` in the graph
    /// or manually disabled, and not re-enabled since.
    pub fn is_disabled(&self) -> bool {
        self.cfg.disabled_at_boot ^ self.handle.flag(flag::DISABLED_FLIP)
    }

    /// Return `true` if the node is held stopped as a dependent of a
    /// deactivated node.
    pub fn is_collateral(&self) -> bool {
        self.handle.flag(flag::COLLATERAL)
    }

    /// Set whether this node is detached from automatic lifecycle management.
    pub fn set_detached(&self, detached: bool) {
        self.handle.flag_put(flag::DETACHED, detached);
    }

    /// `true` unless a resident shell is still finishing its last activation.
    /// The supervisor gates a respawn on it, so a new activation never shares
    /// flags with the previous one's epilogue.
    pub(crate) fn is_settled(&self) -> bool {
        let f = self.handle.flags.load(Ordering::Acquire);
        f & flag::RESIDENT == 0 || f & flag::IDLE != 0
    }

    /// Spawn glue: is a resident shell already running for this node? `Ok(true)`
    /// — parked, restart it in place with [`__sv_start`](Self::__sv_start);
    /// `Ok(false)` — not spawned yet; `Err(Busy)` — still finishing.
    #[doc(hidden)]
    pub fn __sv_resident(&self) -> Result<bool, SpawnError> {
        let f = self.handle.flags.load(Ordering::Acquire);
        match (f & flag::RESIDENT != 0, f & flag::IDLE != 0) {
            (false, _) => Ok(false),
            (true, true) => Ok(true),
            (true, false) => Err(SpawnError::Busy),
        }
    }

    /// Glue support: every `resources:` gate of this node is filled. One out-of-line
    /// copy instead of one inlined critical section per entry per spawn closure.
    #[doc(hidden)]
    #[inline(never)]
    pub fn __sv_gates_filled(&self) -> bool {
        self.cfg.resource_gates.iter().all(|g| g.is_filled())
    }

    /// Spawn glue: the shell was spawned; later activations restart it.
    #[doc(hidden)]
    pub fn __sv_mark_resident(&self) {
        self.handle.flag_set(flag::RESIDENT);
    }

    /// Spawn glue: start the next activation of a parked resident shell.
    #[doc(hidden)]
    pub fn __sv_start(&self) {
        self.handle.flag_clear(flag::IDLE);
        self.handle.resume_wake.signal(());
    }

    /// Resident shell: the activation is over (everything it lent is back, its
    /// exit is recorded). Publish that, then park until the next start.
    #[doc(hidden)]
    pub async fn __sv_park(&self) {
        self.handle.flag_set(flag::IDLE);
        __sv_gate_event();
        // Wait first: a start that landed between the two lines above latched its
        // signal, and consuming it here keeps it out of the next pause.
        loop {
            self.handle.resume_wake.wait().await;
            if !self.handle.flag(flag::IDLE) {
                break;
            }
        }
    }

    /// Return `true` if the node is detached from automatic lifecycle management.
    pub fn is_detached(&self) -> bool {
        self.handle.flag(flag::DETACHED)
    }

    #[cfg(feature = "trace")]
    /// Record the executor task id for this node instance. Stored before the
    /// trace recorders' cached misses are invalidated, so the next poll of
    /// `id` resolves to this node.
    pub fn set_task_id(&self, id: u32) {
        self.handle.task_id.store(id, Ordering::Release);
        trace::bump_adopt_epoch();
    }

    #[cfg(feature = "trace")]
    /// Adopt a spawn token's task id (and name, if enabled) for tracing.
    pub fn adopt<S>(&self, token: &embassy_executor::SpawnToken<S>) {
        let id = token.id();
        #[cfg(embassy_supervisor_trace_v2)]
        let id = trace::task_key(id);
        self.set_task_id(id);
        #[cfg(feature = "metadata-names")]
        self.stamp_name(token);
    }

    #[cfg(feature = "trace")]
    /// Adopt the current task's id for tracing.
    pub async fn adopt_current(&self) {
        self.set_task_id(trace::current_task_id().await);
    }

    #[cfg(feature = "metadata-names")]
    /// Stamp this node's NUL-terminated trace name (see
    /// [`NodeCfg::with_trace_name`]) into the spawn token's task metadata; a
    /// node without one keeps the executor's default name.
    pub fn stamp_name<S>(&self, token: &embassy_executor::SpawnToken<S>) {
        if let Some(name) = self.cfg.trace_name {
            token.metadata().set_name(name);
        }
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

    /// The activation-scoped clears, run immediately before each handoff
    /// (`spawn`, a resident shell's `__sv_start` inside it, or `signal_resume`)
    /// so the new instance's first poll lands on top of them: a fresh beat
    /// stamp and write mark, no pending beat, no status line, no stall strikes.
    /// Every caller runs `reset()` right before, which drops any undelivered
    /// health event (a stop ends a stall without an event, even one no sweep
    /// saw). A spawn that then fails has cleared them too; its retry or fault
    /// follows.
    pub(crate) fn prepare_activation(&self) {
        #[cfg(feature = "liveness-monitor")]
        self.handle.stale_strikes.store(0, Ordering::Release);
        #[cfg(feature = "liveness")]
        {
            self.handle.last_beat.store(
                embassy_time::Instant::now().as_ticks() as u32,
                Ordering::Release,
            );
            #[cfg(all(feature = "coupling-observe", feature = "liveness-monitor"))]
            self.seed_write_mark();
            self.handle.pending_beat.store(false, Ordering::Release);
        }
        #[cfg(feature = "node-status")]
        self.handle.status.lock(|s| s.set(None));
    }

    /// The clears that precede every handoff, with no await in between.
    pub(crate) fn arm(&self) {
        self.reset();
        self.prepare_activation();
    }

    /// Resume a parked `Pause` instance in place and publish it.
    pub(crate) fn resume_in_place(&self) {
        self.arm();
        self.signal_resume();
        self.mark_running();
    }

    /// A bound-stopped node may only come back while every bound provider serves.
    pub(crate) fn bound_ok(&self) -> bool {
        #[cfg(feature = "bound-deps")]
        if self.handle.flag(flag::BOUND_STOPPED) {
            return self.cfg.bound_deps.iter().all(|d| is_serving(d));
        }
        true
    }

    /// Publish the instance just handed off: bump the epoch, then set RUNNING
    /// unless it already acked (a higher tier or the other core ran it to its
    /// exit or park before this call), in which case it stays down. Clears the
    /// handoff bits (`flag::HANDOFF_CLEARS`) in the same update either way.
    pub(crate) fn mark_running(&self) {
        #[cfg(feature = "epochs")]
        {
            self.handle.epoch.fetch_add(1, Ordering::AcqRel);
            self.handle.epoch_wake.signal(());
        }
        let _ = self
            .handle
            .flags
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |f| {
                Some(if f & flag::DROPPED == 0 {
                    (f | flag::RUNNING) & !flag::HANDOFF_CLEARS
                } else {
                    f & !flag::HANDOFF_CLEARS
                })
            });
        #[cfg(all(
            feature = "liveness-monitor",
            feature = "coupling-observe",
            feature = "readiness"
        ))]
        if self.cfg.ready_on_write {
            PROBE_EVT.signal();
        }
    }

    /// Manually disable or re-enable this node.
    pub fn set_disabled(&self, disabled: bool) {
        self.handle
            .flag_put(flag::DISABLED_FLIP, disabled ^ self.cfg.disabled_at_boot);
    }

    pub(crate) fn has_acked_stop(&self) -> bool {
        self.handle.flag(flag::DROPPED) && !self.handle.flag(flag::COMPLETED)
    }

    pub(crate) fn has_dropped(&self) -> bool {
        self.handle.flag(flag::DROPPED)
    }

    pub(crate) fn reset(&self) {
        let stale = flag::SHUTDOWN | flag::DROPPED | flag::BUSY | flag::COMPLETED;
        #[cfg(feature = "readiness")]
        let stale = stale | flag::READY;
        #[cfg(feature = "liveness-monitor")]
        let stale = stale | flag::HEALTH_PENDING;
        // Pending bits belong to the old instance; do not carry them forward.
        #[cfg(feature = "fault-inject")]
        let stale = stale | flag::PENDING_ACK | flag::PENDING_EXIT;
        self.handle.flag_clear(stale);
        #[cfg(feature = "readiness")]
        self.handle.ready_wake.reset();
        self.handle.shutdown_wake.reset();
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
            .field("collateral", &self.is_collateral())
            .field("detached", &self.is_detached());
        #[cfg(feature = "epochs")]
        d.field("epoch", &self.epoch());
        d.finish_non_exhaustive()
    }
}
