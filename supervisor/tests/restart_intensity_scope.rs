//! What `restart_budget:` counts besides the storm itself (restart_intensity.rs):
//! only the verb's own target, never a dependent cycled with it or an
//! `activate`; `run` hands a storm to the app; with the lifecycle queue the
//! refusal is a `Faulted(RestartStorm)` event.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlOp, FaultKind, Supervisor, TaskNode, request_control, supervisor_graph,
};
use embassy_time::{Duration, MockDriver};
use support::settle;

supervisor_graph! {
    node TARGET = Terminate, deps: [], task: target, restart_budget: 1 in 1000;
    node DEPENDENT = Terminate, deps: [TARGET], task: dependent, restart_budget: 1 in 1000;
}

static TARGET_RUNS: AtomicU32 = AtomicU32::new(0);
static DEPENDENT_RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn target(node: &'static TaskNode) {
    TARGET_RUNS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn dependent(node: &'static TaskNode) {
    DEPENDENT_RUNS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.mark_exited();
}

/// Activations so far: (TARGET, DEPENDENT).
fn runs() -> (u32, u32) {
    (
        TARGET_RUNS.load(Ordering::SeqCst),
        DEPENDENT_RUNS.load(Ordering::SeqCst),
    )
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start");
    settle("first runs", || runs() == (1, 1)).await;

    sup.restart(&TARGET, &spawner)
        .await
        .expect("TARGET's one restart of the window");
    settle("TARGET restarted with its dependent", || runs() == (2, 2)).await;
    sup.restart(&DEPENDENT, &spawner)
        .await
        .expect("being cycled as TARGET's dependent did not spend DEPENDENT's budget");
    settle("DEPENDENT restarted", || runs() == (2, 3)).await;

    // A fresh window spent only on activations: none of them may count.
    clock.advance(Duration::from_millis(1001));
    for _ in 0..2 {
        sup.deactivate(&TARGET).await.expect("deactivate");
        sup.activate(&TARGET, &spawner).await.expect("activate");
    }
    settle("activations ran", || runs() == (4, 5)).await;
    sup.restart(&TARGET, &spawner)
        .await
        .expect("activations never count, so the window's one restart is still free");
    settle("free restart ran", || runs() == (5, 6)).await;

    // 2^32 ticks later the window has long closed, whatever a u32 tick counter
    // would make of it.
    clock.advance(Duration::from_ticks(1 << 32));
    sup.restart(&TARGET, &spawner)
        .await
        .expect("a restart 2^32 ticks after the window opened opens a fresh one");
    settle("fresh-window restart ran", || runs() == (6, 7)).await;

    // The window is spent: a Restart through the mailbox is a storm, and `run`
    // returns it instead of absorbing it.
    #[cfg(feature = "lifecycle-events")]
    while embassy_supervisor::try_wait_lifecycle().is_some() {}
    request_control(&TARGET, ControlOp::Restart).await;
    let fault = sup.run(&spawner).await;
    assert!(matches!(fault.kind, FaultKind::RestartStorm), "{fault}");
    assert!(core::ptr::eq(fault.node, &TARGET));
    assert!(
        TARGET.is_running() && DEPENDENT.is_running(),
        "the refused restart stopped nothing"
    );
    assert_eq!(runs(), (6, 7), "nor respawned anything");

    #[cfg(feature = "lifecycle-events")]
    {
        let ev = embassy_supervisor::try_wait_lifecycle().expect("the refusal is evented");
        assert!(core::ptr::eq(ev.node, &TARGET));
        assert!(matches!(
            ev.kind,
            embassy_supervisor::LifecycleKind::Faulted(FaultKind::RestartStorm)
        ));
        assert!(
            embassy_supervisor::try_wait_lifecycle().is_none(),
            "and nothing else moved"
        );
    }

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn only_the_verbs_target_counts_and_run_returns_the_storm() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
}
