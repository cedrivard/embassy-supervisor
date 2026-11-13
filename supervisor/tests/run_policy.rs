//! `run_with` returns the faults its policy accepts, from each source
//! the driver meets: the boot wave, a pool growth and a control `Activate`.
//! `run()` keeps absorbing bring-up faults.
mod support;

use std::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{
    ControlOp, DeferredShrink, FailFast, FaultKind, NodeFault, Origin, ReturnOnHalt, RunPolicy,
    Supervisor, TaskNode, request_control, supervisor_graph,
};
use embassy_time::{Duration, Timer};
use support::{Clock, settle};

static DONE: AtomicBool = AtomicBool::new(false);

async fn idle_worker(node: &'static TaskNode, _slot: &mut u32) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node BOOT = Terminate, task: idle_worker,
        resources: [BOOT_SLOT: u32], slot_timeout: 50;
    node LATE = Terminate, task: idle_worker, disabled,
        resources: [LATE_SLOT: u32], slot_timeout: 50;
    pool GROW = [OnDemand], deps: [],
        task: idle_worker,
        resources: [GROW_SLOT: u32],
        policy: DeferredShrink::new(Duration::from_secs(3)),
        min: 1, max: 1;
}

/// Returns only a missing executor slot: every fault in this graph is a
/// missing resource, so `run_with` must never return.
struct OnlyExecutorSlot;
impl RunPolicy for OnlyExecutorSlot {
    fn returns(&mut self, fault: &NodeFault, _: Origin) -> bool {
        matches!(fault.kind, FaultKind::ExecutorSlotEmpty)
    }
}

/// Takes every fault and records where the last one came from.
#[derive(Default)]
struct Recording {
    origin: Option<Origin>,
}
impl RunPolicy for Recording {
    fn returns(&mut self, _: &NodeFault, origin: Origin) -> bool {
        self.origin = Some(origin);
        true
    }
}

/// `run` must still be driving after 500 ms of mock time.
async fn keeps_driving(run: impl core::future::Future<Output = NodeFault>, what: &str) {
    match select(run, Timer::after(Duration::from_millis(500))).await {
        Either::First(fault) => panic!("{what} returned {fault}"),
        Either::Second(()) => {}
    }
}

fn missing(fault: NodeFault, node: &'static TaskNode) {
    assert!(
        core::ptr::eq(fault.node, node),
        "fault names {}, expected {}",
        fault.node.name(),
        node.name()
    );
    assert!(matches!(fault.kind, FaultKind::ResourceMissing), "{fault}");
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    // The default policy absorbs BOOT's bring-up fault, and so does a policy
    // that accepts another kind only.
    keeps_driving(sup.run(&spawner), "run()").await;
    keeps_driving(
        sup.run_with(&spawner, &mut ReturnOnHalt),
        "run_with(ReturnOnHalt)",
    )
    .await;
    keeps_driving(
        sup.run_with(&spawner, &mut OnlyExecutorSlot),
        "run_with(OnlyExecutorSlot)",
    )
    .await;
    assert!(!BOOT.is_running());

    // Boot wave: FailFast returns BOOT's fault once the wave has settled.
    missing(sup.run_with(&spawner, &mut FailFast).await, &BOOT);
    assert!(!BOOT.is_running() && !BOOT.is_disabled());

    // Pool growth: BOOT comes up, then the pool grows to `min` and faults.
    BOOT_SLOT.provide(1);
    let mut policy = Recording::default();
    missing(sup.run_with(&spawner, &mut policy).await, &GROW[0]);
    assert_eq!(policy.origin, Some(Origin::PoolGrow));
    assert!(BOOT.is_running());
    assert!(!GROW[0].is_running());

    // Control `Activate`: the pool grows this time, then LATE faults.
    GROW_SLOT[0].provide(2);
    request_control(&LATE, ControlOp::Activate).await;
    let mut policy = Recording::default();
    missing(sup.run_with(&spawner, &mut policy).await, &LATE);
    assert_eq!(policy.origin, Some(Origin::Activate));
    settle("pool member up", || GROW[0].is_running()).await;
    assert!(!LATE.is_running() && !LATE.is_disabled());

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn run_with_returns_what_the_policy_accepts() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).expect("spawn driver")),
        &DONE,
        Clock::Free(Duration::from_millis(1)),
        30,
        || {
            format!(
                "BOOT={} LATE={} GROW0={}",
                BOOT.is_running(),
                LATE.is_running(),
                GROW[0].is_running()
            )
        },
    );
}
