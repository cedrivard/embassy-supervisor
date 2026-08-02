//! `Supervisor::run()`: the canonical driver as one call — start() the graph,
//! then drive pool scaling and runtime control until an error. Verifies bring-up
//! happens, a busy floor grows the pool, a Deactivate command cascades, and a
//! wedged node's missed ack surfaces as `RunError::Shutdown` out of `run()`
//! (mock clock advanced by the main thread for the ack timeout). Harness as
//! teardown.rs.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlOp, DeferredShrink, RunError, Supervisor, TaskNode, request_control, supervisor_graph,
};

supervisor_graph! {
    node MAIN = Terminate, deps: [], task: main_worker;
    // Wedged on demand: started by control, never acks its stop.
    node WEDGE = Terminate, deps: [], task: wedge_worker, disabled;
    pool WORKERS = [Terminate, OnDemand], deps: [],
        task: pool_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3600)),
        min: 1, max: 2;
}

static MAIN_SPAWNS: AtomicU32 = AtomicU32::new(0);
static POOL_SPAWNS: AtomicU32 = AtomicU32::new(0);
static WEDGE_SPAWNS: AtomicU32 = AtomicU32::new(0);
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn main_worker(node: &'static TaskNode) {
    MAIN_SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn pool_worker(node: &'static TaskNode) {
    POOL_SPAWNS.fetch_add(1, Ordering::SeqCst);
    node.mark_busy(); // saturate: the policy wants to grow to the ceiling
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// Never observes shutdown, never acks: stopping it times out.
async fn wedge_worker(_node: &'static TaskNode) {
    WEDGE_SPAWNS.fetch_add(1, Ordering::SeqCst);
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

/// The whole app: one `run()` call. Its return value is the test's payload.
#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    match sup.run(spawner).await {
        RunError::Shutdown(e) => {
            assert_eq!(e.node.name, "wedge", "the wedged node is named");
            DONE.store(true, Ordering::SeqCst);
        }
        RunError::Spawn(_) => panic!("bring-up failed"),
    }
}

/// Feeds control commands from "outside" (an app control surface).
#[embassy_executor::task]
async fn scenario() {
    // run() brought the graph up (MAIN + the pool floor) and the busy floor
    // grew the pool to its ceiling.
    settle(|| MAIN_SPAWNS.load(Ordering::SeqCst) == 1).await;
    settle(|| POOL_SPAWNS.load(Ordering::SeqCst) == 2).await;
    PHASE.store(1, Ordering::SeqCst);

    // Control through run(): activate the wedge...
    request_control(&WEDGE, ControlOp::Activate).await;
    settle(|| WEDGE_SPAWNS.load(Ordering::SeqCst) == 1).await;
    PHASE.store(2, Ordering::SeqCst);

    // ...then deactivate it: the cascade's shutdown_and_wait times out (the
    // main thread advances the clock), and run() returns RunError::Shutdown.
    request_control(&WEDGE, ControlOp::Deactivate).await;
    PHASE.store(3, Ordering::SeqCst);
}

#[test]
fn run_drives_pools_and_control_until_error() {
    let clock = embassy_time::MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(runner(spawner).unwrap());
            spawner.spawn(scenario().unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        // Phase 3 parks on the 2 s ack timeout; advance in small repeated steps.
        if PHASE.load(Ordering::SeqCst) >= 3 {
            clock.advance(embassy_time::Duration::from_millis(500));
        }
        assert!(
            StdInstant::now() < deadline,
            "did not complete (phase={}, pool_spawns={})",
            PHASE.load(Ordering::SeqCst),
            POOL_SPAWNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
