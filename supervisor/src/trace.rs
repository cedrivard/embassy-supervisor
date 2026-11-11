#[cfg(feature = "trace-nested")]
use core::cell::Cell;
use core::sync::atomic::Ordering;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use portable_atomic::{AtomicBool, AtomicU32, AtomicUsize};

use crate::{GraphRef, TaskNode};

/// Register `graph` so tracing hooks can resolve task ids to nodes.
pub fn register_graph(graph: &'static GraphRef) {
    graph.register();
}

/// Resolve an executor task id to its node: a linear scan over every registered
/// graph's slots (id 0 = "unknown" is never matched), plus each graph's hidden
/// self-node under `trace-self`. O(total nodes) with the per-graph cap at 256:
/// one atomic load per node, plus one critical section for the chain head and
/// one per graph to follow the chain.
fn node_for(task_id: u32) -> Option<&'static TaskNode> {
    if task_id == 0 {
        return None;
    }
    crate::graphs().find_map(|g| {
        let nodes = g.nodes().iter().copied().flatten();
        #[cfg(feature = "trace-self")]
        let mut nodes = nodes.chain(g.self_node());
        #[cfg(not(feature = "trace-self"))]
        let mut nodes = nodes;
        nodes.find(|n| n.task_id() == task_id)
    })
}

/// Return the id of the currently executing task.
///
/// # Panics
/// Panics when polled outside an embassy executor.
pub async fn current_task_id() -> u32 {
    core::future::poll_fn(|cx| {
        let id = embassy_executor::raw::task_from_waker(cx.waker()).id();
        #[cfg(embassy_supervisor_trace_v2)]
        let id = task_key(id);
        core::task::Poll::Ready(id)
    })
    .await
}

/// Narrow an executor `TaskId` to the recorders' `u32` key.
///
/// The recorders key tasks by the `u32` the embassy-executor 0.10 hooks pass.
/// Under `--cfg embassy_supervisor_trace_v2` the executor hands out `TaskId`s
/// instead, and this is the one place they narrow: the low 32 bits of the
/// task's address, which is exactly the 0.10 id on a 32-bit target. Custom
/// hooks forwarding to the `on_*` recorders go through here.
#[cfg(embassy_supervisor_trace_v2)]
pub fn task_key(id: embassy_executor::TaskId) -> u32 {
    id.get() as u32
}

/// Narrow an `ExecutorId` to the recorders' `u32` key; see [`task_key`].
#[cfg(embassy_supervisor_trace_v2)]
pub fn executor_key(id: embassy_executor::ExecutorId) -> u32 {
    id.get() as u32
}

/// Maximum number of executors that can be traced concurrently.
pub const MAX_EXECUTORS: usize = 4;

struct ExecutorSlot {
    id: AtomicU32,
    current_task: AtomicU32,
    current_begin: AtomicU32,
    idle: AtomicBool,
    idle_since: AtomicU32,
    idle_ticks: AtomicU32,
    exec_ticks: AtomicU32,
    polls: AtomicU32,
    passes: AtomicU32,
    /// The last task id this executor polled that resolved to no node, and
    /// the [`ADOPT_EPOCH`] read before that lookup: while the epoch has not
    /// moved, the id is still nobody's and its polls skip the scan.
    miss_id: AtomicU32,
    miss_epoch: AtomicU32,
    #[cfg(feature = "trace-nested")]
    stolen_ticks: AtomicU32,
}

impl ExecutorSlot {
    const fn new() -> Self {
        Self {
            id: AtomicU32::new(0),
            current_task: AtomicU32::new(0),
            current_begin: AtomicU32::new(0),
            idle: AtomicBool::new(false),
            idle_since: AtomicU32::new(0),
            idle_ticks: AtomicU32::new(0),
            exec_ticks: AtomicU32::new(0),
            polls: AtomicU32::new(0),
            passes: AtomicU32::new(0),
            miss_id: AtomicU32::new(0),
            miss_epoch: AtomicU32::new(0),
            #[cfg(feature = "trace-nested")]
            stolen_ticks: AtomicU32::new(0),
        }
    }
}

/// Bumped by every task-id adoption ([`TaskNode::set_task_id`]) and by every
/// graph's first registration, each after the id or the graph is visible: a
/// miss cached under an older value is looked up again. Starts at 1, so a
/// fresh slot's zeroed cache never matches.
static ADOPT_EPOCH: AtomicU32 = AtomicU32::new(1);

/// Invalidate every executor's cached miss: a task id may resolve now.
pub(crate) fn bump_adopt_epoch() {
    // Release pairs with the hook's Acquire read: a hook that sees the new
    // epoch also sees the id or the graph published before it.
    ADOPT_EPOCH.fetch_add(1, Ordering::Release);
}

#[cfg(feature = "trace-nested")]
/// Maximum number of cores tracked for nested execution.
pub const MAX_CORES: usize = 2;

#[cfg(feature = "trace-nested")]
type CoreIdFn = fn() -> usize;

/// One critical section around a hook's bookkeeping; under `trace-nested`
/// it also guards the core-id fn.
#[cfg(feature = "trace-nested")]
static LOCK: Mutex<CriticalSectionRawMutex, Cell<Option<CoreIdFn>>> = Mutex::new(Cell::new(None));
/// One critical section around a hook's bookkeeping.
#[cfg(not(feature = "trace-nested"))]
static LOCK: Mutex<CriticalSectionRawMutex, ()> = Mutex::new(());

#[cfg(feature = "trace-nested")]
/// Set the function used to determine the current core id for nested tracing.
///
/// Call it before any other core starts an executor, and keep `f` to a
/// register read: it runs inside a critical section on every poll.
pub fn set_core_id_fn(f: fn() -> usize) {
    LOCK.lock(|c| c.set(Some(f)));
}

/// The calling core's preemption stack: the registered core-id fn's answer,
/// core 0 without one. The caller holds `LOCK`.
#[cfg(feature = "trace-nested")]
fn core_of(core_id_fn: &Cell<Option<CoreIdFn>>) -> usize {
    core_id_fn.get().map_or(0, |f| f().min(MAX_CORES - 1))
}

/// Depth of each core's preemption stack. Written only under `LOCK`.
#[cfg(feature = "trace-nested")]
static NEST_DEPTH: [AtomicUsize; MAX_CORES] = [const { AtomicUsize::new(0) }; MAX_CORES];
/// Each core's preemption stack: the executor slot of every poll in flight,
/// innermost last. Written only under `LOCK`.
#[cfg(feature = "trace-nested")]
static NEST_STACK: [[AtomicUsize; MAX_EXECUTORS]; MAX_CORES] =
    [const { [const { AtomicUsize::new(0) }; MAX_EXECUTORS] }; MAX_CORES];

static EXECUTORS: [ExecutorSlot; MAX_EXECUTORS] = [const { ExecutorSlot::new() }; MAX_EXECUTORS];

static LAST_SLOT: AtomicUsize = AtomicUsize::new(0);

fn slot_for(executor_id: u32) -> Option<(usize, &'static ExecutorSlot)> {
    // Fast path: the slot that matched last time (hooks fire thousands of times
    // per second from at most a handful of executors).
    let last = LAST_SLOT.load(Ordering::Relaxed);
    if let Some(s) = EXECUTORS.get(last)
        && s.id.load(Ordering::Acquire) == executor_id
    {
        return Some((last, s));
    }
    // Pass 1: existing slot.
    for (i, s) in EXECUTORS.iter().enumerate() {
        if s.id.load(Ordering::Acquire) == executor_id {
            LAST_SLOT.store(i, Ordering::Relaxed);
            return Some((i, s));
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
                return Some((i, s));
            }
            Err(existing) if existing == executor_id => {
                LAST_SLOT.store(i, Ordering::Relaxed);
                return Some((i, s));
            }
            Err(_) => {}
        }
    }
    None // table full: this executor's events are dropped
}

fn now_ticks() -> u32 {
    embassy_time::Instant::now().as_ticks() as u32
}

/// Add `n` to a counter with one writer: a plain load and store, no
/// read-modify-write.
fn add(counter: &AtomicU32, n: u32) {
    counter.store(
        counter.load(Ordering::Relaxed).wrapping_add(n),
        Ordering::Relaxed,
    );
}

/// Close `slot`'s idle window at `now`, if one is open. The caller holds
/// `LOCK`, as does every other access to the window, so a reader never sees
/// half of the update.
fn end_idle(slot: &ExecutorSlot, now: u32) {
    if slot.idle.load(Ordering::Relaxed) {
        add(
            &slot.idle_ticks,
            now.wrapping_sub(slot.idle_since.load(Ordering::Relaxed)),
        );
        slot.idle.store(false, Ordering::Relaxed);
    }
}

/// Hook: call when an executor starts a poll pass.
pub fn on_poll_start(executor_id: u32) {
    let Some((_, slot)) = slot_for(executor_id) else {
        return;
    };
    add(&slot.passes, 1);
}

/// Hook: call when an executor begins running a task.
pub fn on_task_exec_begin(executor_id: u32, task_id: u32) {
    let Some((idx, slot)) = slot_for(executor_id) else {
        return;
    };
    // The push lands in the same critical section as the window it opens, so
    // a nested poll preempting this hook finds the stack whole.
    #[cfg(feature = "trace-nested")]
    LOCK.lock(|core_id_fn| {
        let now = now_ticks();
        end_idle(slot, now);
        slot.current_begin.store(now, Ordering::Relaxed);
        slot.current_task.store(task_id, Ordering::Release);
        let core = core_of(core_id_fn);
        let depth = NEST_DEPTH[core].load(Ordering::Relaxed);
        NEST_DEPTH[core].store(depth + 1, Ordering::Relaxed);
        if let Some(frame) = NEST_STACK[core].get(depth) {
            frame.store(idx, Ordering::Relaxed);
        }
    });
    #[cfg(not(feature = "trace-nested"))]
    {
        let _ = idx;
        // The clock is read under the lock when it closes an idle window, so
        // the window never ends before a concurrent `executor_stats` read.
        let now = if slot.idle.load(Ordering::Acquire) {
            LOCK.lock(|_| {
                let now = now_ticks();
                end_idle(slot, now);
                now
            })
        } else {
            now_ticks()
        };
        slot.current_begin.store(now, Ordering::Relaxed);
        slot.current_task.store(task_id, Ordering::Release);
    }
}

/// Hook: call when an executor finishes running a task.
pub fn on_task_exec_end(executor_id: u32, task_id: u32) {
    let Some((_, slot)) = slot_for(executor_id) else {
        return;
    };
    // The pop and the parent's charge land in one critical section, so a
    // nested poll preempting this hook neither sees nor leaves half of them.
    #[cfg(feature = "trace-nested")]
    let elapsed = LOCK.lock(|core_id_fn| {
        let raw = now_ticks().wrapping_sub(slot.current_begin.load(Ordering::Relaxed));
        slot.current_task.store(0, Ordering::Release);
        let stolen = slot.stolen_ticks.load(Ordering::Relaxed);
        slot.stolen_ticks.store(0, Ordering::Relaxed);
        let core = core_of(core_id_fn);
        let cur = NEST_DEPTH[core].load(Ordering::Relaxed);
        let depth = cur.saturating_sub(1);
        NEST_DEPTH[core].store(depth, Ordering::Relaxed);
        if depth > 0
            && let Some(frame) = NEST_STACK[core].get(depth - 1)
            && let Some(parent) = EXECUTORS.get(frame.load(Ordering::Relaxed))
        {
            add(&parent.stolen_ticks, raw);
        }
        raw.saturating_sub(stolen)
    });
    #[cfg(not(feature = "trace-nested"))]
    let elapsed = {
        let begin = slot.current_begin.load(Ordering::Relaxed);
        slot.current_task.store(0, Ordering::Release);
        now_ticks().wrapping_sub(begin)
    };
    // One writer from here on: only this executor's own hooks touch these
    // counters, and the node's task runs on this executor.
    add(&slot.exec_ticks, elapsed);
    add(&slot.polls, 1);
    let epoch = ADOPT_EPOCH.load(Ordering::Acquire);
    if slot.miss_id.load(Ordering::Relaxed) == task_id
        && slot.miss_epoch.load(Ordering::Relaxed) == epoch
    {
        return;
    }
    match node_for(task_id) {
        Some(node) => {
            add(&node.handle.exec_ticks, elapsed);
            add(&node.handle.polls, 1);
            if elapsed > node.handle.max_poll_ticks.load(Ordering::Relaxed) {
                node.handle.max_poll_ticks.store(elapsed, Ordering::Relaxed);
            }
        }
        // Stamped with the epoch read before the lookup: an adoption that
        // raced the scan has moved it, so the next poll looks again.
        None => {
            slot.miss_id.store(task_id, Ordering::Relaxed);
            slot.miss_epoch.store(epoch, Ordering::Relaxed);
        }
    }
}

/// Hook: call when an executor becomes idle.
pub fn on_executor_idle(executor_id: u32) {
    let Some((_, slot)) = slot_for(executor_id) else {
        return;
    };
    // No lock: only this executor's own hooks write the window, and closing it
    // (`end_idle`) happens under `LOCK`. Opening it is two stores, the stamp
    // before the flag (Release), so a reader under `LOCK` that sees the flag
    // (Acquire) also sees the stamp, and reads the clock after it: the total
    // never steps back. Taking the cross-core lock here would cost a critical
    // section on every idle pass of every executor.
    if !slot.idle.load(Ordering::Relaxed) {
        slot.idle_since.store(now_ticks(), Ordering::Relaxed);
        slot.idle.store(true, Ordering::Release);
    }
}

/// Hook: call when a task ends. Clears the id from the node that owns it once
/// that node's instance has acked; a node showing no ack is armed for its next
/// activation, which may already run in the same task storage, and keeps the
/// id.
pub fn on_task_end(_executor_id: u32, task_id: u32) {
    if let Some(node) = node_for(task_id)
        && node
            .handle
            .task_id
            .compare_exchange(task_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        && !node.has_dropped()
    {
        // A respawn into the same task storage adopted this id before the
        // clear landed: the node is live again, so the id is still its own.
        if node
            .handle
            .task_id
            .compare_exchange(0, task_id, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // A poll that missed the id while it read 0 cached the miss.
            bump_adopt_epoch();
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
/// Runtime statistics collected for one executor.
pub struct ExecutorStats {
    /// Ticks spent idle.
    pub idle_ticks: u32,
    /// Ticks spent executing tasks.
    pub exec_ticks: u32,
    /// Number of task executions.
    pub polls: u32,
    /// Number of poll passes.
    pub passes: u32,
}

#[cfg(feature = "defmt")]
impl defmt::Format for ExecutorStats {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(
            fmt,
            "idle {} ticks, exec {} ticks, {} polls, {} passes",
            self.idle_ticks,
            self.exec_ticks,
            self.polls,
            self.passes
        );
    }
}

/// Return the current statistics for `executor_id`, if it is registered.
pub fn executor_stats(executor_id: u32) -> Option<ExecutorStats> {
    for s in &EXECUTORS {
        if s.id.load(Ordering::Acquire) == executor_id {
            // Under the lock the hooks take to move the idle window, with the
            // clock read inside it: the total never steps back between calls.
            let idle = LOCK.lock(|_| {
                let idle = s.idle_ticks.load(Ordering::Relaxed);
                if s.idle.load(Ordering::Acquire) {
                    idle.wrapping_add(
                        now_ticks().wrapping_sub(s.idle_since.load(Ordering::Relaxed)),
                    )
                } else {
                    idle
                }
            });
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

/// Return the idle tick count for `executor_id`, or zero if unknown.
pub fn executor_idle_ticks(executor_id: u32) -> u32 {
    executor_stats(executor_id).unwrap_or_default().idle_ticks
}

/// Return the registered executor ids (zero means unoccupied).
pub fn executors() -> [u32; MAX_EXECUTORS] {
    let mut ids = [0u32; MAX_EXECUTORS];
    for (id, s) in ids.iter_mut().zip(&EXECUTORS) {
        *id = s.id.load(Ordering::Acquire);
    }
    ids
}

/// Return the node currently running on `executor_id` and how long its
/// current poll has run, in ticks: `None` when nothing is in flight there or
/// the task in flight belongs to no node.
///
/// Ask from a context that keeps running while that executor is stuck in a
/// poll: a higher-priority executor or interrupt, or another core. A task on
/// the same executor only ever finds its own poll in flight, and a wedged
/// executor never runs the task that would ask. Under `trace-nested` the
/// time taken by nested polls that preempted this one and have returned is
/// left out, so a hog in a higher tier is not blamed on the poll it
/// interrupted.
pub fn current_task(executor_id: u32) -> Option<(&'static TaskNode, u32)> {
    for s in &EXECUTORS {
        if s.id.load(Ordering::Acquire) == executor_id {
            let task_id = s.current_task.load(Ordering::Acquire);
            if task_id == 0 {
                return None;
            }
            let running = now_ticks().wrapping_sub(s.current_begin.load(Ordering::Relaxed));
            #[cfg(feature = "trace-nested")]
            let running = running.saturating_sub(s.stolen_ticks.load(Ordering::Relaxed));
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
