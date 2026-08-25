use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver};

struct Probe;

impl Drop for Probe {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

static DROPPED: AtomicU32 = AtomicU32::new(0);
static CONS_RUNS: AtomicU32 = AtomicU32::new(0);
static LOC_RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn cons_worker(node: &'static TaskNode, _probe: Probe) {
    CONS_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// `local` worker: the `!Send` counter arrives `&mut` and is restored on exit,
/// so the value observed on the next run is the same allocation.
async fn loc_worker(node: &'static TaskNode, counter: &mut Rc<Cell<u32>>) {
    counter.set(counter.get() + 1);
    LOC_RUNS.store(counter.get(), Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node CONS = Terminate, deps: [], task: cons_worker,
        resources: [PROBE: consume Probe];
    node LOC = Terminate, deps: [], task: loc_worker,
        resources: [COUNTER: local Rc<Cell<u32>>];
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

    PROBE.provide(Probe);
    COUNTER.provide(Rc::new(Cell::new(0)));

    sup.start(&spawner).await.expect("start");
    settle(|| CONS_RUNS.load(Ordering::SeqCst) == 1 && LOC_RUNS.load(Ordering::SeqCst) == 1).await;

    sup.teardown().await.expect("teardown");
    assert_eq!(
        DROPPED.load(Ordering::SeqCst),
        1,
        "worker dropped the Probe"
    );
    assert!(PROBE.take().is_none(), "consume leaves the slot empty");

    let err = sup
        .respawn_terminate(&spawner)
        .await
        .expect_err("respawn without a fresh provide must fail");
    assert!(
        matches!(err.kind, FaultKind::ResourceMissing),
        "and must say the slot was never provided, not blame a task pool: {err}"
    );

    PROBE.provide(Probe);
    sup.respawn_terminate(&spawner).await.expect("respawn");
    settle(|| CONS_RUNS.load(Ordering::SeqCst) == 2 && LOC_RUNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        CONS_RUNS.load(Ordering::SeqCst),
        2,
        "consume node respawned"
    );
    assert_eq!(
        LOC_RUNS.load(Ordering::SeqCst),
        2,
        "local respawn re-took the SAME Rc (counter accumulated)"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn resource_kinds_consume_and_local() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    // Tick mock time so the Busy path's `with_timeout(SLOT_READY_TIMEOUT)` can
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (cons={}, loc={}, dropped={})",
            CONS_RUNS.load(Ordering::SeqCst),
            LOC_RUNS.load(Ordering::SeqCst),
            DROPPED.load(Ordering::SeqCst),
        );
        clock.advance(Duration::from_millis(10));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
