//! What `restart_budget:` counts besides the storm itself (restart_intensity.rs):
//! only the verb's own target, never a dependent cycled with it or an
//! `activate`; `run` hands a storm to the app; with the lifecycle queue the
//! refusal is a `Faulted(RestartStorm)` event.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlOp, FaultKind, Supervisor, TaskNode, request_control, supervisor_graph,
};
use embassy_time::{Duration, MockDriver};

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

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
    assert!(f(), "did not settle");
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start");
    settle(|| runs() == (1, 1)).await;

    sup.restart(&TARGET, &spawner)
        .await
        .expect("TARGET's one restart of the window");
    settle(|| runs() == (2, 2)).await;
    sup.restart(&DEPENDENT, &spawner)
        .await
        .expect("being cycled as TARGET's dependent did not spend DEPENDENT's budget");
    settle(|| runs() == (2, 3)).await;

    // A fresh window spent only on activations: none of them may count.
    clock.advance(Duration::from_millis(1001));
    for _ in 0..2 {
        sup.deactivate(&TARGET).await.expect("deactivate");
        sup.activate(&TARGET, &spawner).await.expect("activate");
    }
    settle(|| runs() == (4, 5)).await;
    sup.restart(&TARGET, &spawner)
        .await
        .expect("activations never count, so the window's one restart is still free");
    settle(|| runs() == (5, 6)).await;

    // 2^32 ticks later the window has long closed, whatever a u32 tick counter
    // would make of it.
    clock.advance(Duration::from_ticks(1 << 32));
    sup.restart(&TARGET, &spawner)
        .await
        .expect("a restart 2^32 ticks after the window opened opens a fresh one");
    settle(|| runs() == (6, 7)).await;

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
    let _clock = MockDriver::get();
    let thread = std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| spawner.spawn(driver(spawner).unwrap()));
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        if thread.is_finished() {
            thread.join().expect("executor assertions failed");
            panic!("executor exited before completion");
        }
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
