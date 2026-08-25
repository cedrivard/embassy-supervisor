#[cfg(feature = "trace-nested")]
use core::cell::Cell;
use core::sync::atomic::Ordering;

#[cfg(feature = "trace-nested")]
use embassy_sync::blocking_mutex::Mutex;
#[cfg(feature = "trace-nested")]
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use portable_atomic::{AtomicBool, AtomicU32, AtomicUsize};

use crate::{GraphRef, TaskNode};

/// Register `graph` so tracing hooks can resolve task ids to nodes.
pub fn register_graph(graph: &'static GraphRef) {
    graph.register();
}

/// Resolve an executor task id to its node: a linear scan over every registered
/// graph's slots (id 0 = "unknown" is never matched), plus each graph's hidden
/// self-node under `trace-self`. O(total nodes) with the per-graph cap at 256 —
/// a handful of atomic loads per poll in practice.
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
pub async fn current_task_id() -> u32 {
    core::future::poll_fn(|cx| {
        core::task::Poll::Ready(embassy_executor::raw::task_from_waker(cx.waker()).id())
    })
    .await
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
    #[cfg(feature = "trace-nested")]
    stolen_ticks: AtomicU32,
}

#[allow(clippy::declare_interior_mutable_const)]
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
    #[cfg(feature = "trace-nested")]
    stolen_ticks: AtomicU32::new(0),
};

#[cfg(feature = "trace-nested")]
/// Maximum number of cores tracked for nested execution.
pub const MAX_CORES: usize = 2;

#[cfg(feature = "trace-nested")]
type CoreIdFn = fn() -> usize;
#[cfg(feature = "trace-nested")]
static CORE_ID_FN: Mutex<CriticalSectionRawMutex, Cell<Option<CoreIdFn>>> =
    Mutex::new(Cell::new(None));

#[cfg(feature = "trace-nested")]
/// Set the function used to determine the current core id for nested tracing.
pub fn set_core_id_fn(f: fn() -> usize) {
    CORE_ID_FN.lock(|c| c.set(Some(f)));
}

#[cfg(feature = "trace-nested")]
fn core_id() -> usize {
    CORE_ID_FN
        .lock(Cell::get)
        .map_or(0, |f| f().min(MAX_CORES - 1))
}

#[cfg(feature = "trace-nested")]
static NEST_DEPTH: [AtomicUsize; MAX_CORES] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicUsize = AtomicUsize::new(0);
    [ZERO; MAX_CORES]
};
#[cfg(feature = "trace-nested")]
static NEST_STACK: [[AtomicUsize; MAX_EXECUTORS]; MAX_CORES] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicUsize = AtomicUsize::new(0);
    #[allow(clippy::declare_interior_mutable_const)]
    const ROW: [AtomicUsize; MAX_EXECUTORS] = [ZERO; MAX_EXECUTORS];
    [ROW; MAX_CORES]
};

static EXECUTORS: [ExecutorSlot; MAX_EXECUTORS] = [FREE_SLOT; MAX_EXECUTORS];

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

/// Hook: call when an executor starts a poll pass.
pub fn on_poll_start(executor_id: u32) {
    let Some((_, slot)) = slot_for(executor_id) else {
        return;
    };
    let passes = slot.passes.load(Ordering::Relaxed);
    slot.passes.store(passes.wrapping_add(1), Ordering::Relaxed);
}

/// Hook: call when an executor begins running a task.
pub fn on_task_exec_begin(executor_id: u32, task_id: u32) {
    let Some((idx, slot)) = slot_for(executor_id) else {
        return;
    };
    let now = now_ticks();
    if slot.idle.swap(false, Ordering::AcqRel) {
        let idled = now.wrapping_sub(slot.idle_since.load(Ordering::Acquire));
        slot.idle_ticks.fetch_add(idled, Ordering::Relaxed);
    }
    slot.current_begin.store(now, Ordering::Relaxed);
    slot.current_task.store(task_id, Ordering::Release);
    #[cfg(feature = "trace-nested")]
    {
        let core = core_id();
        let depth = NEST_DEPTH[core].fetch_add(1, Ordering::Relaxed);
        if let Some(frame) = NEST_STACK[core].get(depth) {
            frame.store(idx, Ordering::Relaxed);
        }
    }
    #[cfg(not(feature = "trace-nested"))]
    let _ = idx;
}

/// Hook: call when an executor finishes running a task.
pub fn on_task_exec_end(executor_id: u32, task_id: u32) {
    let Some((_, slot)) = slot_for(executor_id) else {
        return;
    };
    let begin = slot.current_begin.load(Ordering::Relaxed);
    slot.current_task.store(0, Ordering::Release);
    let raw = now_ticks().wrapping_sub(begin);
    #[cfg(feature = "trace-nested")]
    let elapsed = {
        let stolen = slot.stolen_ticks.swap(0, Ordering::Relaxed);
        let core = core_id();
        let cur = NEST_DEPTH[core].load(Ordering::Relaxed);
        let depth = cur.saturating_sub(1);
        if cur > 0 {
            NEST_DEPTH[core].store(depth, Ordering::Relaxed);
        }
        if depth > 0
            && let Some(frame) = NEST_STACK[core].get(depth - 1)
        {
            let parent = frame.load(Ordering::Relaxed);
            if let Some(p) = EXECUTORS.get(parent) {
                p.stolen_ticks.fetch_add(raw, Ordering::Relaxed);
            }
        }
        raw.saturating_sub(stolen)
    };
    #[cfg(not(feature = "trace-nested"))]
    let elapsed = raw;
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

/// Hook: call when an executor becomes idle.
pub fn on_executor_idle(executor_id: u32) {
    let Some((_, slot)) = slot_for(executor_id) else {
        return;
    };
    if !slot.idle.load(Ordering::Acquire) {
        slot.idle_since.store(now_ticks(), Ordering::Relaxed);
        slot.idle.store(true, Ordering::Release);
    }
}

/// Hook: call when a task ends.
pub fn on_task_end(_executor_id: u32, task_id: u32) {
    if let Some(node) = node_for(task_id) {
        let _ =
            node.handle
                .task_id
                .compare_exchange(task_id, 0, Ordering::AcqRel, Ordering::Acquire);
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

/// Return the current statistics for `executor_id`, if it is registered.
pub fn executor_stats(executor_id: u32) -> Option<ExecutorStats> {
    for s in &EXECUTORS {
        if s.id.load(Ordering::Acquire) == executor_id {
            let mut idle = s.idle_ticks.load(Ordering::Relaxed);
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

/// Return the node currently running on `executor_id` and how long it has run.
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
