use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlCommand, ControlOp, FaultKind, Supervisor, TaskNode, supervisor_graph,
};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node TARGET = Terminate, deps: [], task: target, restart_budget: 2 in 1000;
    node FREE = Terminate, deps: [], task: free;
}

static TARGET_STARTS: AtomicU32 = AtomicU32::new(0);
static FREE_STARTS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn target(node: &'static TaskNode) {
    TARGET_STARTS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn free(node: &'static TaskNode) {
    FREE_STARTS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn settle() {
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    let sup = Supervisor::new(&GRAPH);
    assert_eq!(
        TARGET.restart_budget(),
        Some((2, Duration::from_millis(1000)))
    );
    assert_eq!(FREE.restart_budget(), None);

    sup.start(&spawner).await.expect("start");
    settle().await;

    for _ in 0..2 {
        sup.restart(&TARGET, &spawner).await.expect("within budget");
        settle().await;
        clock.advance(Duration::from_millis(100));
    }
    assert_eq!(TARGET_STARTS.load(Ordering::SeqCst), 3);

    let err = sup
        .restart(&TARGET, &spawner)
        .await
        .expect_err("the third restart inside the window is a storm");
    assert!(matches!(err.kind, FaultKind::RestartStorm));
    assert_eq!(err.to_string(), "target: restart budget exhausted");
    settle().await;
    assert!(TARGET.is_running(), "the refused restart stopped nothing");
    assert_eq!(TARGET_STARTS.load(Ordering::SeqCst), 3, "nor respawned it");

    // The window opened at the first restart, 200 ms ago.
    clock.advance(Duration::from_millis(801));
    sup.restart(&TARGET, &spawner)
        .await
        .expect("a closed window opens a fresh budget");
    settle().await;
    assert_eq!(TARGET_STARTS.load(Ordering::SeqCst), 4);

    for _ in 0..5 {
        sup.restart(&FREE, &spawner)
            .await
            .expect("no budget, no limit");
        settle().await;
    }
    assert_eq!(FREE_STARTS.load(Ordering::SeqCst), 6);

    let restart = ControlCommand {
        node: &TARGET,
        op: ControlOp::Restart,
    };
    sup.apply_control(restart, &spawner)
        .await
        .expect("second restart of the new window");
    settle().await;
    let err = sup
        .apply_control(restart, &spawner)
        .await
        .expect_err("the control path propagates a storm instead of absorbing it");
    assert!(matches!(err.kind, FaultKind::RestartStorm));
    assert!(core::ptr::eq(err.node, &TARGET));

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn restart_budget_refuses_a_storm_inside_the_window() {
    let _clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
