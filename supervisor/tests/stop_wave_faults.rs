mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;
use support::settle;

static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn dep_worker(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// The wedge: never observes shutdown, never acks.
async fn wedged_worker(_node: &'static TaskNode) {
    core::future::pending::<()>().await;
}

supervisor_graph! {
    node DEP = Terminate, deps: [], task: dep_worker;
    node WEDGE = Terminate, deps: [DEP], task: wedged_worker;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    settle("DEP and WEDGE running", || {
        DEP.is_running() && WEDGE.is_running()
    })
    .await;
    PHASE.store(1, Ordering::SeqCst);

    let err = sup.teardown().await.expect_err("wedge cannot ack");
    assert_eq!(err.node.name(), "wedge");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout));
    assert!(WEDGE.is_running(), "the wedge stays marked running");
    assert!(
        DEP.is_running() && !DEP.shutdown_requested(),
        "the aborting wave never signalled the wedge's dependency: it keeps \
         serving under the wedge"
    );
    PHASE.store(2, Ordering::SeqCst);

    let err = sup
        .teardown_continue()
        .await
        .expect_err("the wedge is still the reported fault");
    assert_eq!(err.node.name(), "wedge");
    assert!(WEDGE.is_running(), "given up on, not resolved");
    assert!(
        DEP.shutdown_requested() && !DEP.is_running(),
        "the give-up released the dependency's signal and its ack was collected"
    );

    DONE.store(true, Ordering::SeqCst);
}

/// Moves the mock clock 5 ms per executor pass once the stop phases begin, so
/// device time passes only while the executor runs: a descheduled executor
/// thread cannot sleep through either 2 s ack window.
#[embassy_executor::task]
async fn clock_ticker() {
    let clock = MockDriver::get();
    while !DONE.load(Ordering::SeqCst) {
        if PHASE.load(Ordering::SeqCst) >= 1 {
            clock.advance(embassy_time::Duration::from_millis(5));
        }
        embassy_futures::yield_now().await;
    }
}

#[test]
fn a_wedged_dependent_holds_then_releases_its_dependency() {
    support::run(
        |spawner| {
            spawner.spawn(driver(spawner).unwrap());
            spawner.spawn(clock_ticker().unwrap());
        },
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (phase={}, dep_running={}, wedge_running={})",
                PHASE.load(Ordering::SeqCst),
                DEP.is_running(),
                WEDGE.is_running(),
            )
        },
    );
}
