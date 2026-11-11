//! A control command queued while the driver's pools pass is stopping a pool
//! member waits for that stop to finish: the command never cancels the shrink,
//! so a member that never acks surfaces as the shrink's own fault, before the
//! command is applied.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{
    ControlOp, DeferredShrink, FaultKind, Supervisor, TaskNode, request_control, supervisor_graph,
};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node OTHER = Terminate, deps: [], task: other;
    pool CREW = [Terminate, OnDemand], deps: [], task: crew,
        policy: DeferredShrink::new(embassy_time::Duration::from_millis(100)), min: 1, max: 2;
}

static IDLE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn until(cond: impl Fn() -> bool) {
    while !cond() {
        embassy_futures::yield_now().await;
    }
}

async fn yields(n: usize) {
    for _ in 0..n {
        embassy_futures::yield_now().await;
    }
}

/// Busy until the script says otherwise; the surplus member never acks its stop.
async fn crew(node: &'static TaskNode) {
    node.mark_busy();
    until(|| IDLE.load(Ordering::SeqCst)).await;
    node.mark_idle();
    if core::ptr::eq(node, &CREW[1]) {
        core::future::pending::<()>().await;
    }
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn other(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn script() {
    let clock = MockDriver::get();
    until(|| CREW[0].is_busy() && CREW[1].is_running() && CREW[1].is_busy()).await;
    IDLE.store(true, Ordering::SeqCst);
    yields(32).await;
    // Past the 100 ms shrink cooldown: the pools pass signals crew1 and waits
    // for its ack.
    clock.advance(Duration::from_millis(150));
    yields(32).await;
    request_control(&OTHER, ControlOp::Deactivate).await;
    for _ in 0..40 {
        clock.advance(Duration::from_millis(100));
        yields(8).await;
    }
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    match select(sup.run(&spawner), script()).await {
        Either::First(fault) => {
            assert!(
                matches!(fault.kind, FaultKind::ShutdownTimeout),
                "the shrink's missed ack: {fault}"
            );
            assert_eq!(fault.node.name(), "crew1");
            assert!(
                OTHER.is_running(),
                "the queued Deactivate waited for the shrink"
            );
            DONE.store(true, Ordering::SeqCst);
        }
        Either::Second(()) => panic!("the wedged shrink never surfaced"),
    }
}

#[test]
fn a_command_never_cancels_a_pool_stop_in_progress() {
    support::run(
        |spawner| spawner.spawn(runner(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
}
