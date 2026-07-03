//! Trace-hook observability (feature `trace`): a batteries-included consumer for
//! embassy-executor's `_embassy_trace_*` instrumentation hooks.
//!
//! The executor (with its `trace` feature, enabled by this crate's `trace`) calls a
//! set of `extern "Rust"` hooks on every poll, identifying tasks only by an opaque
//! `u32` id. This module supplies what the raw hooks lack:
//!
//!   * **id → node resolution** — the spawn glue `supervisor_graph!` generates
//!     captures each `SpawnToken`'s id into its [`TaskNode`] before spawning
//!     ([`TaskNode::set_task_id`]), so a hook can attribute a poll to a node by
//!     scanning the registered graph (O(N), N ≤ 256). Because the id is overwritten
//!     on every (re)spawn, the mapping stays correct across respawns with no
//!     unlinking — unlike an external task tracker.
//!   * **per-node accounting** — accumulated poll time ([`TaskNode::exec_ticks`]),
//!     poll count ([`TaskNode::poll_count`]), and the longest single poll
//!     ([`TaskNode::max_poll_ticks`], the "never yields" watermark).
//!   * **per-executor accounting** — idle time ([`executor_idle_ticks`]) and the
//!     in-flight poll ([`current_task`] / [`stalled_task`]).
//!
//! With the companion `trace-hooks` feature, `supervisor_graph!` also *defines*
//! the seven `no_mangle` hook symbols at the graph declaration site (they cannot
//! live in this crate: `#![forbid(unsafe_code)]`, and `#[unsafe(no_mangle)]` is
//! an unsafe attribute) — exactly one definition may exist per binary, so an
//! application with its own hooks enables only `trace` and forwards to the
//! recorder fns here instead.
//!
//! ## Semantics and limitations
//!
//!   * All counters are wrapping `u32`s of **embassy-time ticks**. Consumers sample
//!     twice and `wrapping_sub` the readings to compute a rate over their own
//!     window; the crate does no windowing of its own.
//!   * Accounting is **preemption-naive**: on systems with interrupt executors, a
//!     thread-executor poll that gets preempted silently absorbs the preemptor's
//!     CPU time, and idle is tracked per executor, not per core. Hardware-ISR time
//!     is likewise invisible: during a poll it inflates that node, between polls it
//!     lands in the unattributed share.
//!   * Executor busy% exceeds the sum of per-node CPU% by a **per-poll accounting
//!     gap**: executor bookkeeping between `exec_end` and the next `exec_begin`,
//!     plus these hooks' own cost (two `Instant::now()` reads + the O(N) id scan),
//!     is busy time attributed to no node — order 10 µs per poll, so it grows with
//!     poll rate (measured ~13% of a 150 MHz core at ~8k polls/s under HTTP load).
//!     [`ExecutorStats`] measures it directly: `overhead = busy - exec_ticks`, and
//!     `exec_ticks - Σ node exec` separates the unsupervised-task share from it.
//!     Empty scheduler passes (interrupt wakeups that poll nothing) are counted as
//!     idle, not overhead — see [`ExecutorStats`].
//!   * At most [`MAX_EXECUTORS`] executors are tracked (first come, first served);
//!     hooks from further executors are dropped.
//!   * Parked nodes (no `spawn:`) and verbatim-closure `spawn:` forms are not
//!     auto-mapped — call [`TaskNode::set_task_id`] with the token id yourself.
//!
//! Docs: executor trace hooks: `embassy-executor/src/raw/trace.rs` (the hook ids
//! are documented as implementation details, so this module pins to the executor
//! minor version the crate already requires).

use core::cell::Cell;
use core::sync::atomic::Ordering;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use portable_atomic::{AtomicBool, AtomicU32, AtomicUsize};

use crate::TaskNode;

// ─── Graph registry ──────────────────────────────────────────────────────
//
// The registered node slice. A `static` can't hold a plain `&[..]` set at
// runtime, and the crate is `forbid(unsafe_code)` (so no ptr+len atomics with a
// `from_raw_parts` read); a blocking mutex around a `Cell` of the slice is the
// safe equivalent — each access is one short critical section, entered once per
// poll on the recording path.

static NODES: Mutex<CriticalSectionRawMutex, Cell<&'static [Option<&'static TaskNode>]>> =
    Mutex::new(Cell::new(&[]));

/// Register the supervised node slots so the hook recorders can resolve task ids
/// to nodes. Called automatically by [`Supervisor::start`](crate::Supervisor::start)
/// with `GRAPH.nodes`; idempotent (last registration wins).
pub fn register_graph(nodes: &'static [Option<&'static TaskNode>]) {
    NODES.lock(|cell| cell.set(nodes));
}

/// The registered node slots (empty before `register_graph`).
fn nodes() -> &'static [Option<&'static TaskNode>] {
    NODES.lock(Cell::get)
}

/// Resolve an executor task id to its node: a linear scan over the registered
/// slots (id 0 = "unknown" is never matched). O(N) with N ≤ 256 — a handful of
/// atomic loads per poll in practice.
fn node_for(task_id: u32) -> Option<&'static TaskNode> {
    if task_id == 0 {
        return None;
    }
    nodes()
        .iter()
        .flatten()
        .find(|n| n.task_id() == task_id)
        .copied()
}

// ─── Per-executor slots ──────────────────────────────────────────────────

/// Maximum number of executors tracked (thread executors + interrupt executors).
/// Slots are claimed first come, first served; hooks from executors beyond the
/// cap are silently dropped.
pub const MAX_EXECUTORS: usize = 4;

/// Live accounting for one executor. All fields are atomics: hooks may fire from
/// interrupt-priority executors, so no locks anywhere on the recording path.
struct ExecutorSlot {
    /// The executor id owning this slot (`0` = free). Ids are the executor's
    /// address bits, so `0` never collides with a real executor.
    id: AtomicU32,
    /// Task id currently inside `exec_begin..exec_end` (`0` = none).
    current_task: AtomicU32,
    /// Tick at which the current poll began.
    current_begin: AtomicU32,
    /// True between `executor_idle` and the next `poll_start`.
    idle: AtomicBool,
    /// Tick at which the executor went idle.
    idle_since: AtomicU32,
    /// Accumulated idle ticks (wrapping).
    idle_ticks: AtomicU32,
    /// Accumulated in-poll ticks (wrapping): EVERY `exec_begin..exec_end` window,
    /// whether or not the task id resolves to a supervised node. `busy - exec`
    /// is therefore pure executor overhead (bookkeeping + hook cost + ISR time
    /// between polls), and `exec - sum(node exec_ticks)` is the unsupervised-task
    /// share.
    exec_ticks: AtomicU32,
    /// Task polls on this executor (wrapping), supervised or not.
    polls: AtomicU32,
    /// Scheduler passes (`poll_start` events, wrapping). `polls / passes` is the
    /// mean number of task polls per pass.
    passes: AtomicU32,
}

#[allow(clippy::declare_interior_mutable_const)] // const used only as array initializer
const FREE_SLOT: ExecutorSlot = ExecutorSlot {
    id: AtomicU32::new(0),
    current_task: AtomicU32::new(0),
    current_begin: AtomicU32::new(0),
    idle: AtomicBool::new(false),
    idle_since: AtomicU32::new(0),
    idle_ticks: AtomicU32::new(0),
    exec_ticks: AtomicU32::new(0),
    polls: AtomicU32::new(0),
    passes: AtomicU32::new(0),
};

static EXECUTORS: [ExecutorSlot; MAX_EXECUTORS] = [FREE_SLOT; MAX_EXECUTORS];

/// Index of the slot matched by the most recent `slot_for` — the fast path for
/// the overwhelmingly common case (one executor, or one hot executor firing
/// most events). An index, not a pointer: reconstructing a reference from an
/// `AtomicPtr` would need `unsafe`, which this crate forbids.
static LAST_SLOT: AtomicUsize = AtomicUsize::new(0);

/// Find (or claim) the slot for an executor id. Claiming races are settled by
/// `compare_exchange` on the `id` field; a loser retries the scan once via the
/// outer loop shape below (two passes are enough: either it finds the winner's
/// slot or claims another).
fn slot_for(executor_id: u32) -> Option<&'static ExecutorSlot> {
    // Fast path: the slot that matched last time (hooks fire thousands of times
    // per second from at most a handful of executors).
    let last = LAST_SLOT.load(Ordering::Relaxed);
    if let Some(s) = EXECUTORS.get(last)
        && s.id.load(Ordering::Acquire) == executor_id
    {
        return Some(s);
    }
    // Pass 1: existing slot.
    for (i, s) in EXECUTORS.iter().enumerate() {
        if s.id.load(Ordering::Acquire) == executor_id {
            LAST_SLOT.store(i, Ordering::Relaxed);
            return Some(s);
        }
    }
    // Pass 2: claim a free one (or discover a racing claimer of the same id).
    for (i, s) in EXECUTORS.iter().enumerate() {
        match s
            .id
            .compare_exchange(0, executor_id, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                LAST_SLOT.store(i, Ordering::Relaxed);
                return Some(s);
            }
            Err(existing) if existing == executor_id => {
                LAST_SLOT.store(i, Ordering::Relaxed);
                return Some(s);
            }
            Err(_) => {}
        }
    }
    None // table full: this executor's events are dropped
}

/// Current time in embassy-time ticks, truncated to u32 (wrapping arithmetic
/// everywhere makes the truncation harmless for deltas).
fn now_ticks() -> u32 {
    embassy_time::Instant::now().as_ticks() as u32
}

// ─── Recorders ───────────────────────────────────────────────────────────
//
// The `trace-hooks` symbols below forward here; an application defining its own
// hook symbols calls these directly. Everything is lock-free and safe from
// interrupt context.

/// Record a `poll_start` event: counts the scheduler pass — nothing else, by
/// design. An open idle window is closed lazily by the first `exec_begin` of the
/// pass (reusing the timestamp that hook takes anyway), so an **empty pass** —
/// the executor woken by an interrupt with nothing runnable, which can happen
/// hundreds of thousands of times per second — costs no timer read here and
/// merges into the surrounding idle window instead of inflating "overhead" with
/// the instrument's own cost.
pub fn on_poll_start(executor_id: u32) {
    let Some(slot) = slot_for(executor_id) else {
        return;
    };
    slot.passes.fetch_add(1, Ordering::Relaxed);
}

/// Record a task poll starting (`task_exec_begin`). Also closes an open idle
/// window (see [`on_poll_start`]) with the same timestamp — a real poll pays for
/// exactly one timer read here.
pub fn on_task_exec_begin(executor_id: u32, task_id: u32) {
    let Some(slot) = slot_for(executor_id) else {
        return;
    };
    let now = now_ticks();
    if slot.idle.swap(false, Ordering::AcqRel) {
        let idled = now.wrapping_sub(slot.idle_since.load(Ordering::Acquire));
        slot.idle_ticks.fetch_add(idled, Ordering::Relaxed);
    }
    slot.current_begin.store(now, Ordering::Relaxed);
    slot.current_task.store(task_id, Ordering::Release);
}

/// Record a task poll ending (`task_exec_end`): attributes the elapsed ticks to
/// the node mapped to `task_id` (unknown ids are counted nowhere and ignored).
pub fn on_task_exec_end(executor_id: u32, task_id: u32) {
    let Some(slot) = slot_for(executor_id) else {
        return;
    };
    let begin = slot.current_begin.load(Ordering::Relaxed);
    slot.current_task.store(0, Ordering::Release);
    let elapsed = now_ticks().wrapping_sub(begin);
    // Executor-level accounting counts EVERY poll, resolvable or not, so that
    // `busy - exec` isolates pure executor overhead and `exec - sum(nodes)` the
    // unsupervised-task share (see `ExecutorStats`).
    slot.exec_ticks.fetch_add(elapsed, Ordering::Relaxed);
    slot.polls.fetch_add(1, Ordering::Relaxed);
    if let Some(node) = node_for(task_id) {
        node.handle.exec_ticks.fetch_add(elapsed, Ordering::Relaxed);
        node.handle.polls.fetch_add(1, Ordering::Relaxed);
        node.handle
            .max_poll_ticks
            .fetch_max(elapsed, Ordering::Relaxed);
    }
}

/// Record the executor going idle (`executor_idle`): opens an idle window —
/// unless one is already open (an empty pass, whose window was never closed), in
/// which case the original window simply keeps running: no timer read, no store.
/// Hooks of one executor never race each other (they fire from that executor's
/// own context), so the load-then-store is not a lost-update hazard; the
/// `idle_since`-before-`idle` order keeps readers ([`executor_stats`]) safe.
pub fn on_executor_idle(executor_id: u32) {
    let Some(slot) = slot_for(executor_id) else {
        return;
    };
    if !slot.idle.load(Ordering::Acquire) {
        slot.idle_since.store(now_ticks(), Ordering::Relaxed);
        slot.idle.store(true, Ordering::Release);
    }
}

/// Record a task ending for good (`task_end`, i.e. its future completed and the
/// storage is being released): clears the node's task-id mapping so a stale id
/// can't be matched by a later, unrelated task reusing the storage.
pub fn on_task_end(_executor_id: u32, task_id: u32) {
    if let Some(node) = node_for(task_id) {
        // Only clear if it still holds this id (a respawn may have overwritten it).
        let _ =
            node.handle
                .task_id
                .compare_exchange(task_id, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

// ─── Read API ────────────────────────────────────────────────────────────

/// A snapshot of one executor's accounting. All fields are wrapping u32 tick /
/// event counters — sample twice and `wrapping_sub` for rates. The decomposition
/// over a sampling window of `dt` ticks:
///
/// ```text
/// busy      = dt - Δidle_ticks          (executor not sleeping)
/// in-poll   = Δexec_ticks               (inside task polls, supervised or not)
/// overhead  = busy - Δexec_ticks        (executor bookkeeping + trace-hook cost
///                                        + ISR time landing between polls)
/// unsupervised = Δexec_ticks - Σ Δnode.exec_ticks()   (task polls that resolve
///                                        to no supervised node)
/// ```
///
/// **Empty scheduler passes count as idle**, not overhead: the idle window stays
/// open across a pass that polls nothing (see [`on_poll_start`]), because such a
/// wakeup is ~100 ns uninstrumented and timestamping it would make the trace
/// hooks themselves the dominant "overhead". `Δpasses` vs `Δpolls` still shows
/// the empty-wakeup rate explicitly.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecutorStats {
    /// Accumulated idle ticks (includes a currently-open idle window, so a
    /// sleeping executor doesn't read as busy between samples).
    pub idle_ticks: u32,
    /// Accumulated in-poll ticks across ALL task polls on this executor.
    pub exec_ticks: u32,
    /// Task polls (supervised or not).
    pub polls: u32,
    /// Scheduler passes (`poll_start` events); `polls / passes` = polls per pass.
    pub passes: u32,
}

/// Snapshot an executor's accounting. Returns `None` for an untracked id.
pub fn executor_stats(executor_id: u32) -> Option<ExecutorStats> {
    for s in &EXECUTORS {
        if s.id.load(Ordering::Acquire) == executor_id {
            let mut idle = s.idle_ticks.load(Ordering::Relaxed);
            // Include the currently-open idle window so a mostly-idle executor
            // doesn't read as 0% idle between polls.
            if s.idle.load(Ordering::Acquire) {
                idle = idle
                    .wrapping_add(now_ticks().wrapping_sub(s.idle_since.load(Ordering::Relaxed)));
            }
            return Some(ExecutorStats {
                idle_ticks: idle,
                exec_ticks: s.exec_ticks.load(Ordering::Relaxed),
                polls: s.polls.load(Ordering::Relaxed),
                passes: s.passes.load(Ordering::Relaxed),
            });
        }
    }
    None
}

/// Accumulated idle ticks of an executor (wrapping; sample twice for a rate).
/// Returns 0 for an untracked executor id. Shorthand for
/// [`executor_stats`]`.idle_ticks`.
pub fn executor_idle_ticks(executor_id: u32) -> u32 {
    executor_stats(executor_id).unwrap_or_default().idle_ticks
}

/// The executor ids currently tracked (`0` = free slot).
pub fn executors() -> [u32; MAX_EXECUTORS] {
    let mut ids = [0u32; MAX_EXECUTORS];
    for (id, s) in ids.iter_mut().zip(&EXECUTORS) {
        *id = s.id.load(Ordering::Acquire);
    }
    ids
}

/// The node currently being polled by an executor, with how long the poll has
/// been running (ticks). `None` when the executor is idle/between polls, isn't
/// tracked, or the in-flight task isn't a supervised node.
///
/// This is the raw "who is in-flight" primitive behind [`stalled_task`]. Note the
/// single-executor blind spot: a task blocking *this* executor also blocks any
/// observer task on it — run the observer on another (e.g. interrupt-priority)
/// executor, or check from a pre-watchdog-reset path.
pub fn current_task(executor_id: u32) -> Option<(&'static TaskNode, u32)> {
    for s in &EXECUTORS {
        if s.id.load(Ordering::Acquire) == executor_id {
            let task_id = s.current_task.load(Ordering::Acquire);
            if task_id == 0 {
                return None;
            }
            let running = now_ticks().wrapping_sub(s.current_begin.load(Ordering::Relaxed));
            return node_for(task_id).map(|n| (n, running));
        }
    }
    None
}

/// Blocked-task detector: the node whose current poll has exceeded
/// `threshold_ticks`, if any. A poll is expected to take microseconds; one
/// running for, say, >100 ms means the task is busy-looping or computing without
/// an await point and is starving its executor. See [`current_task`] for where
/// this can meaningfully be called from; [`TaskNode::max_poll_ticks`] gives the
/// same information post-hoc without an observer.
pub fn stalled_task(executor_id: u32, threshold_ticks: u32) -> Option<(&'static TaskNode, u32)> {
    current_task(executor_id).filter(|(_, running)| *running >= threshold_ticks)
}

// NOTE on the hook symbols: embassy-executor declares the `_embassy_trace_*`
// hooks as `unsafe extern "Rust"`, so a definition requires `#[unsafe(no_mangle)]`
// — which this crate cannot contain (`#![forbid(unsafe_code)]`, a published
// guarantee). The definitions are therefore emitted by `supervisor_graph!` into
// the APPLICATION crate under the `trace-hooks` feature (one graph declaration,
// one hook set), forwarding to the recorder fns above. An application defining
// its own hooks enables only `trace` and forwards manually.
