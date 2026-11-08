use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlOp, DeferredShrink, FaultKind, Pool, Supervisor, TaskNode, request_control,
    supervisor_graph,
};

supervisor_graph! {
    node MAIN = Terminate, deps: [], task: main_worker;
    node WEDGE = Terminate, deps: [], task: wedge_worker, disabled;
    pool WORKERS = [Terminate, OnDemand], deps: [],
        task: pool_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
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
    node.mark_busy();
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn wedge_worker(_node: &'static TaskNode) {
    WEDGE_SPAWNS.fetch_add(1, Ordering::SeqCst);
    core::future::pending::<()>().await;
}

async fn settle(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
    panic!("never held: {what}");
}

/// The whole app: one `run()` call. Its return value is the test's payload.
#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    let fault = sup.run(&spawner).await;
    assert!(
        matches!(fault.kind, FaultKind::ShutdownTimeout),
        "a wedged node surfaces as a missed ack, not a bring-up failure: {fault}"
    );
    assert_eq!(fault.node.name(), "wedge", "the wedged node is named");
    DONE.store(true, Ordering::SeqCst);
}

#[embassy_executor::task]
async fn scenario() {
    settle("start() spawned MAIN", || {
        MAIN_SPAWNS.load(Ordering::SeqCst) == 1
    })
    .await;
    settle(
        "run()'s pool arm grew the OnDemand member past the floor",
        || POOL_SPAWNS.load(Ordering::SeqCst) == 2 && WORKERS[1].is_running(),
    )
    .await;
    PHASE.store(1, Ordering::SeqCst);

    // Both members idle: the pool arm arms its 1 s shrink cooldown, and reaps
    // the OnDemand member once mock time passes it.
    WORKERS[0].mark_idle();
    WORKERS[1].mark_idle();
    settle("the pool arm armed the shrink cooldown", || {
        WORKERS_POOL.deferred_until().is_some()
    })
    .await;
    embassy_time::MockDriver::get().advance(embassy_time::Duration::from_millis(1001));
    settle("run() reaped the idle OnDemand member", || {
        !WORKERS[1].is_running() && WORKERS[1].has_exited()
    })
    .await;
    assert!(WORKERS[0].is_running(), "the floor member is never reaped");
    PHASE.store(2, Ordering::SeqCst);

    WORKERS[0].mark_busy();
    settle("run() regrew the reaped member under load", || {
        POOL_SPAWNS.load(Ordering::SeqCst) == 3 && WORKERS[1].is_running()
    })
    .await;
    PHASE.store(3, Ordering::SeqCst);

    request_control(&WEDGE, ControlOp::Activate).await;
    settle("the control arm spawned WEDGE", || {
        WEDGE_SPAWNS.load(Ordering::SeqCst) == 1
    })
    .await;
    PHASE.store(4, Ordering::SeqCst);

    request_control(&WEDGE, ControlOp::Deactivate).await;
    PHASE.store(5, Ordering::SeqCst);
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
        // Phase 5 parks on WEDGE's 2 s ack window; advance in small repeated steps.
        if PHASE.load(Ordering::SeqCst) >= 5 {
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
