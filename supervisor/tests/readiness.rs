//! Behavioral tests for the `readiness` feature: a `ready`-marked dep holds a
//! dependent's spawn until the dep's task asserts `set_ready()` (the dependent's
//! first poll proves the ordering), a never-ready dep turns the spawn into
//! `SpawnError::Busy` once the mock clock passes `slot_timeout`, pool growth
//! defers while a ready-marked dep is un-ready, and `clear_ready()` never stops
//! an already-running dependent (status, not control). Harness as teardown.rs.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::{SpawnError, Spawner};
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;

supervisor_graph! {
    node PROVIDER = Terminate, deps: [], task: provider_worker;
    node CONSUMER = Terminate, deps: [PROVIDER ready], task: consumer_worker,
        slot_timeout: 60000;
    // The never-ready case: NEVER is parked (no spawn:) so nothing ever asserts
    // readiness; LATE is started by hand via start_node and must time out.
    node NEVER = Terminate, deps: [];
    node LATE = Terminate, deps: [NEVER ready], task: late_worker, disabled;
    pool WORKERS = [Terminate, OnDemand], deps: [PROVIDER ready],
        task: pool_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
}

/// Provider parks before asserting readiness until the test releases it.
static RELEASE_PROVIDER: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// True iff the consumer's body observed `PROVIDER.is_ready()` on its first poll.
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
/// the provider was ready when the consumer got its first poll — bring-up must
/// have held the spawn until then.
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
    let _err = sup.run_pools(spawner).await;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    // ── start() parks on CONSUMER's ready dep until the provider asserts.
    //    Release it from a helper condition: the executor polls PROVIDER while
    //    start() awaits, so the rendezvous resolves inside this call. ─────────
    RELEASE_PROVIDER.signal(());
    sup.start(spawner).await.expect("start");
    settle(|| CONSUMER_SPAWNS.load(Ordering::SeqCst) == 1).await;
    assert!(
        CONSUMER_SAW_READY.load(Ordering::SeqCst),
        "consumer's first poll saw the provider ready — spawn was held"
    );
    PHASE.store(1, Ordering::SeqCst);

    // ── pool growth defers while a ready-marked dep is un-ready ─────────────
    settle(|| POOL_SPAWNS.load(Ordering::SeqCst) == 1).await; // floor is up
    PROVIDER.clear_ready();
    spawner.spawn(pool_driver(spawner).unwrap());
    // The busy floor makes the policy want to grow; the un-ready dep defers it.
    embassy_supervisor::request_scale();
    for _ in 0..300 {
        embassy_futures::yield_now().await;
    }
    assert_eq!(
        POOL_SPAWNS.load(Ordering::SeqCst),
        1,
        "growth deferred while the ready-marked dep is un-ready"
    );
    // clear_ready is status, not control: nothing already running was stopped.
    assert!(CONSUMER.is_running(), "clear_ready never stops dependents");
    assert!(WORKERS[0].is_running(), "floor member untouched");

    // Re-assert readiness: the next evaluation grows the second member.
    PROVIDER.set_ready();
    embassy_supervisor::request_scale();
    settle(|| POOL_SPAWNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        POOL_SPAWNS.load(Ordering::SeqCst),
        2,
        "growth resumed once the dep re-asserted readiness"
    );
    PHASE.store(2, Ordering::SeqCst);

    // ── never-ready dep: start_node times out with Busy (mock clock advanced
    //    by the main thread while we park here; LATE's slot_timeout = 100 ms
    //    default? no — LATE declares none, so the 100 ms default applies) ─────
    let err = sup
        .start_node(&LATE, spawner)
        .await
        .expect_err("NEVER never asserts readiness");
    assert!(matches!(err, SpawnError::Busy));
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
        // clock in small repeated steps (always landing after the timer arms).
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
