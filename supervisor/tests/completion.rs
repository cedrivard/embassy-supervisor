use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{ControlCommand, ControlOp, Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

supervisor_graph! {
    node ONESHOT = Terminate, deps: [], task: oneshot_worker, pool_size: 2;
    node ACKED = Terminate, deps: [], spawn: acked_task;
    node WEDGED = Terminate, deps: [], spawn: wedged_task;
}

static ONESHOT_RUNS: AtomicU32 = AtomicU32::new(0);
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn oneshot_worker(_node: &'static TaskNode) {
    ONESHOT_RUNS.fetch_add(1, Ordering::SeqCst);
}

/// A hand-written `spawn:` task on the documented contract: on shutdown it calls
/// `mark_exited()` rather than a bare `ack_dropped()`, so its exit is recorded as
/// a completion too.
#[embassy_executor::task]
async fn acked_task(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

#[embassy_executor::task]
async fn wedged_task(_node: &'static TaskNode) {
    core::future::pending::<()>().await;
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

    // ── start: ONESHOT runs and returns; the shell records the completion ───
    sup.start(&spawner).await.expect("start");
    settle(|| ONESHOT_RUNS.load(Ordering::SeqCst) == 1).await;
    settle(|| !ONESHOT.is_running()).await;
    assert!(ONESHOT.has_exited(), "clean return recorded by the shell");
    assert!(
        !ONESHOT.is_running(),
        "returned worker no longer reads as running"
    );
    assert!(
        ONESHOT.has_exited() && !ONESHOT.shutdown_requested(),
        "no shutdown was requested: this is an autonomous completion"
    );
    PHASE.store(1, Ordering::SeqCst);

    // ── teardown skips the exited node (frozen clock: an awaited ack would
    //    hang the test) and returns the wedge as an error via the ack timeout.
    //    Do the happy half first: stop ACKED alone. ────────────────────────────
    sup.stop_node(&ACKED).await.expect("acked node acks");
    assert!(ACKED.has_exited(), "mark_exited on the spawn: contract");
    assert!(
        ACKED.shutdown_requested(),
        "shutdown flag persists: this reads as an acked stop, not autonomous"
    );
    PHASE.store(2, Ordering::SeqCst);

    // ── control Activate respawns the completed Terminate node ──────────────
    sup.apply_control(
        ControlCommand {
            node: &ONESHOT,
            op: ControlOp::Activate,
        },
        &spawner,
    )
    .await
    .expect("activate cascade has nothing to stop");
    settle(|| ONESHOT_RUNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        ONESHOT_RUNS.load(Ordering::SeqCst),
        2,
        "completed node was respawned by Activate"
    );
    settle(|| !ONESHOT.is_running()).await;
    assert!(ONESHOT.has_exited(), "second run recorded too");
    PHASE.store(3, Ordering::SeqCst);

    // ── stop_node on the wedge: Err names the node, which stays running ─────
    let err = sup
        .stop_node(&WEDGED)
        .await
        .expect_err("wedged task cannot ack");
    assert_eq!(err.node.name(), "wedged");
    assert!(
        WEDGED.is_running(),
        "a node that missed its ack stays marked running"
    );
    PHASE.store(4, Ordering::SeqCst);

    // ── teardown_continue: visits everything past the wedge, reports it last ─
    let err = sup
        .teardown_continue()
        .await
        .expect_err("wedge reported after visiting all nodes");
    assert_eq!(err.node.name(), "wedged");
    // Plain teardown now also errors on the wedge (nothing else is running).
    let err = sup.teardown().await.expect_err("wedge still wedged");
    assert_eq!(err.node.name(), "wedged");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn completion_observed_and_timeouts_are_errors() {
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
        if PHASE.load(Ordering::SeqCst) >= 3 {
            clock.advance(embassy_time::Duration::from_millis(500));
        }
        assert!(
            StdInstant::now() < deadline,
            "did not complete (phase={}, oneshot_runs={})",
            PHASE.load(Ordering::SeqCst),
            ONESHOT_RUNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
