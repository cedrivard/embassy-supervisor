use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlCommand, ControlOp, FaultKind, Supervisor, TaskNode, request_control, supervisor_graph,
};
use embassy_time::{Duration, MockDriver};

static CONTROL_RUNS: AtomicU32 = AtomicU32::new(0);
static FIRST_RUNS: AtomicU32 = AtomicU32::new(0);
static SECOND_RUNS: AtomicU32 = AtomicU32::new(0);
static PEER_RUNS: AtomicU32 = AtomicU32::new(0);
static LEAF_RUNS: AtomicU32 = AtomicU32::new(0);
static QUEUED_RUNS: AtomicU32 = AtomicU32::new(0);
static GATE_WAIT: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn control_worker(node: &'static TaskNode, _slot: &mut u32) {
    CONTROL_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn first_worker(node: &'static TaskNode, _slot: &mut u32) {
    FIRST_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn second_worker(node: &'static TaskNode, _slot: &mut u32) {
    SECOND_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn peer_worker(node: &'static TaskNode) {
    PEER_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn leaf_worker(node: &'static TaskNode) {
    LEAF_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn queued_worker(node: &'static TaskNode, _slot: &mut u32) {
    QUEUED_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node CONTROL = Terminate, task: control_worker, disabled,
        resources: [CONTROL_SLOT: u32], slot_timeout: 100;
    node FIRST = Terminate, task: first_worker, disabled,
        resources: [FIRST_SLOT: u32], slot_timeout: 100;
    node SECOND = Terminate, deps: [FIRST], task: second_worker, disabled,
        resources: [SECOND_SLOT: u32], slot_timeout: 100;
    node PEER = Terminate, task: peer_worker, disabled;
    node LEAF = Terminate, deps: [SECOND, PEER], task: leaf_worker, disabled;
    node QUEUED = Terminate, task: queued_worker, disabled,
        resources: [QUEUED_SLOT: u32], slot_timeout: 100;
}

async fn settle(mut condition: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if condition() {
            return;
        }
        embassy_futures::yield_now().await;
    }
    assert!(condition(), "worker entries did not settle");
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("disabled graph bring-up");
    for node in [&CONTROL, &FIRST, &SECOND, &PEER, &LEAF, &QUEUED] {
        assert!(!node.is_running() && node.is_disabled());
    }
    GATE_WAIT.store(true, Ordering::SeqCst);
    let result = sup
        .apply_control(
            ControlCommand {
                node: &CONTROL,
                op: ControlOp::Activate,
            },
            &spawner,
        )
        .await;
    GATE_WAIT.store(false, Ordering::SeqCst);
    let fault = result.expect_err("Activate must return the missing resource fault");
    assert!(core::ptr::eq(fault.node, &CONTROL));
    assert!(matches!(fault.kind, FaultKind::ResourceMissing));
    assert!(!CONTROL.is_running());
    assert!(!CONTROL.is_disabled());
    assert_eq!(CONTROL_RUNS.load(Ordering::SeqCst), 0);

    CONTROL_SLOT.provide(1);
    sup.apply_control(
        ControlCommand {
            node: &CONTROL,
            op: ControlOp::Activate,
        },
        &spawner,
    )
    .await
    .expect("activate CONTROL after providing resource");
    settle(|| CONTROL_RUNS.load(Ordering::SeqCst) == 1).await;
    assert!(CONTROL.is_running());

    GATE_WAIT.store(true, Ordering::SeqCst);
    let fault = sup
        .activate(&LEAF, &spawner)
        .await
        .expect_err("FIRST and SECOND resources absent");
    GATE_WAIT.store(false, Ordering::SeqCst);
    assert!(
        core::ptr::eq(fault.node, &FIRST),
        "first fault wins: {fault}"
    );
    assert!(matches!(fault.kind, FaultKind::ResourceMissing));
    settle(|| PEER_RUNS.load(Ordering::SeqCst) == 1 && LEAF_RUNS.load(Ordering::SeqCst) == 1).await;
    assert!(!FIRST.is_running() && !SECOND.is_running());
    assert_eq!(FIRST_RUNS.load(Ordering::SeqCst), 0);
    assert_eq!(SECOND_RUNS.load(Ordering::SeqCst), 0);
    assert!(PEER.is_running() && LEAF.is_running());

    FIRST_SLOT.provide(1);
    SECOND_SLOT.provide(1);
    sup.activate(&LEAF, &spawner)
        .await
        .expect("repair LEAF dependencies");
    settle(|| FIRST_RUNS.load(Ordering::SeqCst) == 1 && SECOND_RUNS.load(Ordering::SeqCst) == 1)
        .await;
    for node in [&FIRST, &SECOND, &PEER, &LEAF] {
        assert!(node.is_running());
    }
    sup.activate(&LEAF, &spawner)
        .await
        .expect("already-running LEAF is a no-op");
    for counter in [
        &CONTROL_RUNS,
        &FIRST_RUNS,
        &SECOND_RUNS,
        &PEER_RUNS,
        &LEAF_RUNS,
    ] {
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "successful workers must not restart"
        );
    }

    request_control(&QUEUED, ControlOp::Activate).await;
    GATE_WAIT.store(true, Ordering::SeqCst);
    let fault = sup.run(&spawner).await;
    GATE_WAIT.store(false, Ordering::SeqCst);
    assert!(core::ptr::eq(fault.node, &QUEUED));
    assert!(matches!(fault.kind, FaultKind::ResourceMissing));
    assert!(!QUEUED.is_running() && !QUEUED.is_disabled());
    assert_eq!(QUEUED_RUNS.load(Ordering::SeqCst), 0);

    sup.teardown()
        .await
        .expect("successful workers acknowledge teardown");
    for node in [&CONTROL, &FIRST, &SECOND, &PEER, &LEAF, &QUEUED] {
        assert!(!node.is_running());
    }
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn activation_returns_startup_faults() {
    let clock = MockDriver::get();
    let thread = std::thread::spawn(|| {
        let executor = Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| spawner.spawn(driver(spawner).unwrap()));
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        if thread.is_finished() {
            thread.join().expect("executor assertions failed");
            panic!("executor exited before completion");
        }
        assert!(
            StdInstant::now() < deadline,
            "activation scenario did not complete"
        );
        if GATE_WAIT.load(Ordering::SeqCst) {
            clock.advance(Duration::from_millis(10));
        }
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
