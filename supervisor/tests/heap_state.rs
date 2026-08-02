//! Behavioral tests for `state: Type = init_expr` (feature `heap-state`): every
//! activation allocates exactly one fresh Box, every exit frees it (net zero
//! across N cycles — reclaimable heap, no accumulation), the worker sees its
//! per-activation `&mut Type`, and an allocation failure surfaces as
//! `SpawnError::Busy` from the spawn glue — retryable once the allocator
//! recovers. A counting/denying `#[global_allocator]` wraps the system one.
//! Harness as teardown.rs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::{SpawnError, Spawner};
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

/// The state type, sized to be unmistakable in the allocator's ledger.
const STATE_SIZE: usize = 4096;

pub struct Scratch {
    buf: [u8; STATE_SIZE],
    cursor: usize,
}

supervisor_graph! {
    node CRUNCH = Terminate, deps: [], task: crunch_worker, pool_size: 2,
        state: Scratch = Scratch { buf: [0; STATE_SIZE], cursor: 0 }, disabled;
}

/// Bytes currently allocated at exactly `STATE_SIZE` (the state Boxes), and a
/// switch that denies those allocations to model OOM.
static STATE_BYTES_LIVE: AtomicI64 = AtomicI64::new(0);
static DENY_STATE_ALLOC: AtomicBool = AtomicBool::new(false);

struct Ledger;
// SAFETY: delegates to System; the bookkeeping is atomic counters only.
unsafe impl GlobalAlloc for Ledger {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == core::mem::size_of::<Scratch>() {
            if DENY_STATE_ALLOC.load(Ordering::SeqCst) {
                return core::ptr::null_mut();
            }
            STATE_BYTES_LIVE.fetch_add(layout.size() as i64, Ordering::SeqCst);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() == core::mem::size_of::<Scratch>() {
            STATE_BYTES_LIVE.fetch_sub(layout.size() as i64, Ordering::SeqCst);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static LEDGER: Ledger = Ledger;

static RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// Uses its per-activation state and returns; the shell drops the Box first
/// thing after this returns, then records the exit.
async fn crunch_worker(_node: &'static TaskNode, state: &mut Scratch) {
    assert_eq!(state.cursor, 0, "every activation gets a FRESH state");
    state.buf[0] = 0xA5;
    state.cursor = 1;
    RUNS.fetch_add(1, Ordering::SeqCst);
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(spawner).await.expect("start (CRUNCH disabled)");
    assert_eq!(STATE_BYTES_LIVE.load(Ordering::SeqCst), 0, "nothing yet");

    // N activate/exit cycles: one alloc per activation, freed by the exit —
    // net zero, no accumulation.
    for cycle in 1..=5u32 {
        sup.start_node(&CRUNCH, spawner).await.expect("activate");
        settle(|| RUNS.load(Ordering::SeqCst) == cycle).await;
        settle(|| !CRUNCH.is_running()).await;
        assert!(CRUNCH.has_exited());
        assert_eq!(
            STATE_BYTES_LIVE.load(Ordering::SeqCst),
            0,
            "cycle {cycle}: state Box freed on exit"
        );
    }
    assert_eq!(RUNS.load(Ordering::SeqCst), 5);

    // OOM: the glue's fallible boxing maps a null alloc to SpawnError::Busy —
    // nothing spawned, nothing leaked, and a later retry succeeds.
    DENY_STATE_ALLOC.store(true, Ordering::SeqCst);
    let err = sup
        .start_node(&CRUNCH, spawner)
        .await
        .expect_err("denied allocation");
    assert!(matches!(err, SpawnError::Busy));
    assert!(!CRUNCH.is_running());
    assert_eq!(STATE_BYTES_LIVE.load(Ordering::SeqCst), 0);

    DENY_STATE_ALLOC.store(false, Ordering::SeqCst);
    sup.start_node(&CRUNCH, spawner).await.expect("retry works");
    settle(|| RUNS.load(Ordering::SeqCst) == 6).await;
    settle(|| !CRUNCH.is_running()).await;
    assert_eq!(STATE_BYTES_LIVE.load(Ordering::SeqCst), 0);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn state_boxes_are_reclaimed_and_fallible() {
    let _clock = embassy_time::MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (runs={}, live={})",
            RUNS.load(Ordering::SeqCst),
            STATE_BYTES_LIVE.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
