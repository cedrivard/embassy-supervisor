//! The policy of `run_with` is called once for every fault of a cascade, in
//! order, not only for the cascade's first fault: an `Activate` wave and the
//! way back up of a `Restart` each raise two faults, and a policy taking the
//! second one gets it.
mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlOp, FaultKind, NodeFault, Origin, RunPolicy, Supervisor, TaskNode, request_control,
    supervisor_graph,
};
use embassy_time::Duration;
use support::{Clock, settle};

static DONE: AtomicBool = AtomicBool::new(false);
static R_RUNS: AtomicU32 = AtomicU32::new(0);

/// Takes the `n`th fault it sees, counting every call.
struct Nth {
    n: u32,
    seen: u32,
    origins: [Option<Origin>; 4],
}
impl Nth {
    fn new(n: u32) -> Self {
        Self {
            n,
            seen: 0,
            origins: [None; 4],
        }
    }
}
impl RunPolicy for Nth {
    fn returns(&mut self, _: &NodeFault, origin: Origin) -> bool {
        self.origins[self.seen as usize] = Some(origin);
        self.seen += 1;
        self.seen == self.n
    }
}

async fn idle(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn with_slot(node: &'static TaskNode, _slot: &mut u32) {
    idle(node).await
}

/// Asserts readiness on its first run only, so dependents gated on it fault
/// after a restart.
async fn provider(node: &'static TaskNode) {
    if R_RUNS.fetch_add(1, Ordering::SeqCst) == 0 {
        node.set_ready();
    }
    idle(node).await
}

supervisor_graph! {
    executor NEVER;
    node A = Terminate, task: with_slot, disabled,
        resources: [A_SLOT: u32], slot_timeout: 50;
    node B = Terminate, executor: NEVER, task: idle, disabled, slot_timeout: 50;
    node C = Terminate, deps: [A, B], task: idle, disabled;
    node R = Terminate, task: provider;
    node D1 = Terminate, deps: [R ready], task: idle, slot_timeout: 50;
    node D2 = Terminate, deps: [R ready], task: idle, slot_timeout: 50;
}

fn names(fault: &NodeFault, node: &'static TaskNode) {
    assert!(
        core::ptr::eq(fault.node, node),
        "fault names {}, expected {}",
        fault.node.name(),
        node.name()
    );
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    // Activate C, taking the first fault: `run_with` returns A's, and the
    // wave still reports B's fault to the policy after it.
    request_control(&C, ControlOp::Activate).await;
    let mut policy = Nth::new(1);
    let fault = sup.run_with(&spawner, &mut policy).await;
    names(&fault, &A);
    assert!(matches!(fault.kind, FaultKind::ResourceMissing), "{fault}");
    assert_eq!(
        policy.seen, 2,
        "a fault after a taken one is still reported"
    );
    // Latch A and B down so the next boot wave leaves them alone; the next
    // Activate clears the latches again.
    sup.deactivate(&A).await.expect("A stops");
    sup.deactivate(&B).await.expect("B stops");

    // Activate C: its deps A (no resource) and B (no executor) both fault in
    // one wave. Taking the second fault returns B's.
    request_control(&C, ControlOp::Activate).await;
    let mut policy = Nth::new(2);
    let fault = sup.run_with(&spawner, &mut policy).await;
    names(&fault, &B);
    assert!(
        matches!(fault.kind, FaultKind::ExecutorSlotEmpty),
        "{fault}"
    );
    assert_eq!(policy.seen, 2, "one call per fault");
    assert_eq!(
        policy.origins[..2],
        [Some(Origin::Activate), Some(Origin::Activate)]
    );
    settle("D1 and D2 up", || D1.is_running() && D2.is_running()).await;

    // Latch A and B down so the next boot wave leaves them alone.
    sup.deactivate(&A).await.expect("A stops");
    sup.deactivate(&B).await.expect("B stops");

    // Restart R: it comes back without readiness, so D1 and D2 both time out
    // on the way back up. Taking the second fault returns D2's.
    request_control(&R, ControlOp::Restart).await;
    let mut policy = Nth::new(2);
    let fault = sup.run_with(&spawner, &mut policy).await;
    names(&fault, &D2);
    assert!(
        matches!(fault.kind, FaultKind::ReadyDepTimeout { .. }),
        "{fault}"
    );
    assert_eq!(policy.seen, 2, "one call per fault");
    assert_eq!(
        policy.origins[..2],
        [Some(Origin::Restart), Some(Origin::Restart)]
    );
    assert!(R.is_running());
    assert_eq!(R_RUNS.load(Ordering::SeqCst), 2);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn every_fault_of_a_cascade_reaches_the_policy() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        Clock::Free(Duration::from_millis(1)),
        30,
        || {
            format!(
                "A={} B={} C={} R={} D1={} D2={}",
                A.is_running(),
                B.is_running(),
                C.is_running(),
                R.is_running(),
                D1.is_running(),
                D2.is_running()
            )
        },
    );
}
