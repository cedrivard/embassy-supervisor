use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{DeferredShrink, FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;

supervisor_graph! {
    node PROVIDER = Terminate, deps: [], task: provider_worker;
    node CONSUMER = Terminate, deps: [PROVIDER ready], task: consumer_worker,
        slot_timeout: 60000;
    node NEVER = Terminate, deps: [];
    node LATE = Terminate, deps: [NEVER ready], task: late_worker, disabled;
    pool WORKERS = [Terminate, OnDemand], deps: [PROVIDER ready],
        task: pool_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
}

static RELEASE_PROVIDER: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static CONSUMER_SAW_READY: AtomicBool = AtomicBool::new(false);
static CONSUMER_SPAWNS: AtomicU32 = AtomicU32::new(0);
static POOL_SPAWNS: AtomicU32 = AtomicU32::new(0);
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn provider_worker(node: &'static TaskNode) {
    RELEASE_PROVIDER.wait().await;
    node.set_ready();
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// The rendezvous proof lives here: this body's first statement records whether
async fn consumer_worker(node: &'static TaskNode) {
    CONSUMER_SAW_READY.store(PROVIDER.is_ready(), Ordering::SeqCst);
    CONSUMER_SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn late_worker(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn pool_worker(node: &'static TaskNode) {
    POOL_SPAWNS.fetch_add(1, Ordering::SeqCst);
    // Stay busy forever so the policy always wants to grow the second member.
    node.mark_busy();
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn pool_driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    let _err = sup.run_pools(&spawner).await;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    // ── start() parks on CONSUMER's ready dep until the provider asserts.
    RELEASE_PROVIDER.signal(());
    sup.start(&spawner).await.expect("start");
    settle(|| CONSUMER_SPAWNS.load(Ordering::SeqCst) == 1).await;
    assert!(
        CONSUMER_SAW_READY.load(Ordering::SeqCst),
        "consumer's first poll saw the provider ready — spawn was held"
    );
    PHASE.store(1, Ordering::SeqCst);

    settle(|| POOL_SPAWNS.load(Ordering::SeqCst) == 1).await;
    PROVIDER.clear_ready();
    spawner.spawn(pool_driver(spawner).unwrap());
    embassy_supervisor::request_scale();
    for _ in 0..300 {
        embassy_futures::yield_now().await;
    }
    assert_eq!(
        POOL_SPAWNS.load(Ordering::SeqCst),
        1,
        "growth deferred while the ready-marked dep is un-ready"
    );
    assert!(CONSUMER.is_running(), "clear_ready never stops dependents");
    assert!(WORKERS[0].is_running(), "floor member untouched");

    PROVIDER.set_ready();
    embassy_supervisor::request_scale();
    settle(|| POOL_SPAWNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        POOL_SPAWNS.load(Ordering::SeqCst),
        2,
        "growth resumed once the dep re-asserted readiness"
    );
    PHASE.store(2, Ordering::SeqCst);

    let err = sup
        .start_node(&LATE, &spawner)
        .await
        .expect_err("NEVER never asserts readiness");
    assert!(
        matches!(err.kind, FaultKind::ReadyDepTimeout { dep } if dep.name() == "never"),
        "the fault names the dep that never asserted: {err}"
    );
    assert_eq!(err.node.name(), "late", "and the node that could not start");
    assert!(!LATE.is_running());

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn ready_deps_gate_bringup_and_growth() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        // Phase 2 parks on LATE's 100 ms ready-dep timeout; advance the frozen
        if PHASE.load(Ordering::SeqCst) >= 2 {
            clock.advance(embassy_time::Duration::from_millis(50));
        }
        assert!(
            StdInstant::now() < deadline,
            "did not complete (phase={}, consumer_spawns={}, pool_spawns={})",
            PHASE.load(Ordering::SeqCst),
            CONSUMER_SPAWNS.load(Ordering::SeqCst),
            POOL_SPAWNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
