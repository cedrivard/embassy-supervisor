//! A policy that absorbs `ShutdownTimeout` with a wedged pool member: the
//! failed stop is retried once per driver wake, not in a loop, and a queued
//! control command is still served.
mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{
    ControlOp, NodeFault, Origin, PoolStats, RunPolicy, ScaleAction, ScalingPolicy, Supervisor,
    TaskNode, request_control, supervisor_graph,
};
use embassy_time::{Duration, Instant, Timer};
use support::Clock;

static DONE: AtomicBool = AtomicBool::new(false);
static DECIDES: AtomicU32 = AtomicU32::new(0);

/// Asks for a shrink on every pass: the worst case for a wedged member.
struct AlwaysShrink;
impl ScalingPolicy for AlwaysShrink {
    fn decide(&self, _: PoolStats, _: Instant) -> ScaleAction {
        DECIDES.fetch_add(1, Ordering::SeqCst);
        ScaleAction::Shrink
    }
}

/// Absorbs every fault.
struct NeverReturn;
impl RunPolicy for NeverReturn {
    fn returns(&mut self, _: &NodeFault, _: Origin) -> bool {
        false
    }
}

/// Never acks a shutdown.
async fn wedged(_node: &'static TaskNode) {
    core::future::pending::<()>().await
}

async fn late(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    pool WORKERS = [OnDemand], deps: [],
        task: wedged,
        policy: AlwaysShrink = AlwaysShrink,
        min: 0, max: 1, ack_timeout: 50;
    node LATE = Terminate, task: late, disabled;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start_node(&WORKERS[0], &spawner)
        .await
        .expect("member up");

    let script = async {
        // No wake arrives: one pass, one missed ack, then the driver waits.
        Timer::after(Duration::from_millis(500)).await;
        let idle = DECIDES.load(Ordering::SeqCst);
        assert_eq!(idle, 1, "the failed stop was retried without a wake");

        // A command is served within one ack window.
        let asked = Instant::now();
        request_control(&LATE, ControlOp::Activate).await;
        while !LATE.is_running() {
            assert!(
                Instant::now() - asked < Duration::from_millis(200),
                "control starved behind the wedged member"
            );
            Timer::after(Duration::from_millis(5)).await;
        }
        Timer::after(Duration::from_millis(500)).await;
        let after = DECIDES.load(Ordering::SeqCst) - idle;
        assert!(after <= 2, "{after} passes for one command");
    };
    match select(sup.run_with(&spawner, &mut NeverReturn), script).await {
        Either::First(fault) => panic!("run_with returned {fault}"),
        Either::Second(()) => {}
    }
    assert!(
        WORKERS[0].is_running(),
        "a wedged member stays marked running"
    );
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn absorbed_ack_timeout_does_not_loop() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        Clock::Free(Duration::from_millis(1)),
        30,
        || format!("decides={}", DECIDES.load(Ordering::SeqCst)),
    );
}
