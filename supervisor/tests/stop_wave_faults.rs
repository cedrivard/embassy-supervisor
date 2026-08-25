use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

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
    sup.start(&spawner).await.expect("bring-up");
    settle(|| DEP.is_running() && WEDGE.is_running()).await;
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

#[test]
fn a_wedged_dependent_holds_then_releases_its_dependency() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        // Both stop phases park on the 2 s ack window; advance the frozen
        // clock in small repeated steps so an advance always lands after the
        // window's timer arms, and small enough that a scheduling stall never
        if PHASE.load(Ordering::SeqCst) >= 1 {
            clock.advance(embassy_time::Duration::from_millis(250));
        }
        assert!(
            StdInstant::now() < deadline,
            "did not complete (phase={}, dep_running={}, wedge_running={})",
            PHASE.load(Ordering::SeqCst),
            DEP.is_running(),
            WEDGE.is_running(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
