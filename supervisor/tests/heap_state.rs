use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{FaultKind, Supervisor, TaskNode, Zeroable, supervisor_graph};

const STATE_SIZE: usize = 4096;

pub struct Scratch {
    buf: [u8; STATE_SIZE],
    cursor: usize,
}

const ZEROED_SIZE: usize = 2048;

pub struct Zeroed {
    buf: [u8; ZEROED_SIZE],
}
unsafe impl Zeroable for Zeroed {}

supervisor_graph! {
    node CRUNCH = Terminate, deps: [], task: crunch_worker, pool_size: 2,
        state: Scratch = Scratch { buf: [0; STATE_SIZE], cursor: 0 }, disabled;
    node ZERO = Terminate, deps: [], task: zero_worker, pool_size: 2,
        state: zeroed Zeroed, disabled;
}

static STATE_BYTES_LIVE: AtomicI64 = AtomicI64::new(0);
static DENY_STATE_ALLOC: AtomicBool = AtomicBool::new(false);

fn tracked(layout: Layout) -> bool {
    layout.size() == core::mem::size_of::<Scratch>()
        || layout.size() == core::mem::size_of::<Zeroed>()
}

struct Ledger;
unsafe impl GlobalAlloc for Ledger {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if tracked(layout) {
            if DENY_STATE_ALLOC.load(Ordering::SeqCst) {
                return core::ptr::null_mut();
            }
            STATE_BYTES_LIVE.fetch_add(layout.size() as i64, Ordering::SeqCst);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if tracked(layout) {
            STATE_BYTES_LIVE.fetch_sub(layout.size() as i64, Ordering::SeqCst);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static LEDGER: Ledger = Ledger;

static RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

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

static ZERO_RUNS: AtomicU32 = AtomicU32::new(0);

/// Finds its state all-zero on EVERY activation and dirties it, so a reused
/// block would fail the next check.
async fn zero_worker(_node: &'static TaskNode, state: &mut Zeroed) {
    assert!(
        state.buf.iter().all(|&b| b == 0),
        "zeroed state arrives zeroed"
    );
    state.buf.fill(0xFF);
    ZERO_RUNS.fetch_add(1, Ordering::SeqCst);
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start (CRUNCH disabled)");
    assert_eq!(STATE_BYTES_LIVE.load(Ordering::SeqCst), 0, "nothing yet");

    for cycle in 1..=5u32 {
        sup.start_node(&CRUNCH, &spawner).await.expect("activate");
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

    DENY_STATE_ALLOC.store(true, Ordering::SeqCst);
    let err = sup
        .start_node(&CRUNCH, &spawner)
        .await
        .expect_err("denied allocation");
    assert!(
        matches!(err.kind, FaultKind::Spawn(_)),
        "a refused state allocation is a rejected spawn: {err}"
    );
    assert_eq!(err.node.name(), "crunch");
    assert!(!CRUNCH.is_running());
    assert_eq!(STATE_BYTES_LIVE.load(Ordering::SeqCst), 0);

    DENY_STATE_ALLOC.store(false, Ordering::SeqCst);
    sup.start_node(&CRUNCH, &spawner)
        .await
        .expect("retry works");
    settle(|| RUNS.load(Ordering::SeqCst) == 6).await;
    settle(|| !CRUNCH.is_running()).await;
    assert_eq!(STATE_BYTES_LIVE.load(Ordering::SeqCst), 0);

    for cycle in 1..=3u32 {
        sup.start_node(&ZERO, &spawner).await.expect("activate");
        settle(|| ZERO_RUNS.load(Ordering::SeqCst) == cycle).await;
        settle(|| !ZERO.is_running()).await;
        assert_eq!(STATE_BYTES_LIVE.load(Ordering::SeqCst), 0, "cycle {cycle}");
    }
    DENY_STATE_ALLOC.store(true, Ordering::SeqCst);
    let err = sup
        .start_node(&ZERO, &spawner)
        .await
        .expect_err("denied allocation");
    assert!(matches!(err.kind, FaultKind::Spawn(_)), "{err}");
    assert!(!ZERO.is_running());
    DENY_STATE_ALLOC.store(false, Ordering::SeqCst);

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
