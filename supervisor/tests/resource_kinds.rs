mod support;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_time::Duration;
use support::settle;

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

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    PROBE.provide(Probe);
    // SAFETY: called on the slot's executor: `LOC` runs on this one.
    unsafe { COUNTER.provide(Rc::new(Cell::new(0))) };

    sup.start(&spawner).await.expect("start");
    settle("first runs", || {
        CONS_RUNS.load(Ordering::SeqCst) == 1 && LOC_RUNS.load(Ordering::SeqCst) == 1
    })
    .await;

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
    settle("second runs", || {
        CONS_RUNS.load(Ordering::SeqCst) == 2 && LOC_RUNS.load(Ordering::SeqCst) == 2
    })
    .await;
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
    // Tick mock time so the empty consume slot's gate wait (the 100 ms default
    // `slot_timeout`) can expire; the other waits are already satisfied and never park.
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Free(Duration::from_millis(10)),
        10,
        || {
            format!(
                "did not complete (cons={}, loc={}, dropped={})",
                CONS_RUNS.load(Ordering::SeqCst),
                LOC_RUNS.load(Ordering::SeqCst),
                DROPPED.load(Ordering::SeqCst),
            )
        },
    );
}
