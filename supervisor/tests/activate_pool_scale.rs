//! `activate` pokes the pool driver even when its wave returns a fault: the
//! pool of the provider it did bring up grows with nothing else to wake it.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{DeferredShrink, FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver};

static CREW_SPAWNS: AtomicU32 = AtomicU32::new(0);
/// Set while BROKEN's gate waits out its `slot_timeout`.
static ADVANCE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn provider(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn broken(node: &'static TaskNode, _slot: &mut u32) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn crew(node: &'static TaskNode) {
    CREW_SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node PROVIDER = Terminate, deps: [], task: provider, disabled;
    node BROKEN = Terminate, deps: [PROVIDER], task: broken, disabled,
        resources: [MISSING: u32];
    pool CREW = [OnDemand, OnDemand], deps: [PROVIDER], task: crew,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3600)),
        min: 0, max: 2;
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
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner)
        .await
        .expect("everything is disabled or on demand: an empty wave");
    let script = async {
        // The pool driver's first pass declines its Grow: PROVIDER is down.
        for _ in 0..50 {
            embassy_futures::yield_now().await;
        }
        assert_eq!(
            CREW_SPAWNS.load(Ordering::SeqCst),
            0,
            "no member before its provider"
        );
        ADVANCE.store(true, Ordering::SeqCst);
        let fault = sup
            .activate(&BROKEN, &spawner)
            .await
            .expect_err("BROKEN's slot is empty");
        ADVANCE.store(false, Ordering::SeqCst);
        assert!(
            core::ptr::eq(fault.node, &BROKEN) && matches!(fault.kind, FaultKind::ResourceMissing),
            "{fault}"
        );
        assert!(
            PROVIDER.is_running(),
            "the wave still brought the dependency up"
        );
        // No readiness, load mark or policy deadline wakes the parked driver:
        // the grow can only come from activate's own poke.
        settle(|| CREW_SPAWNS.load(Ordering::SeqCst) == 1).await;
        DONE.store(true, Ordering::SeqCst);
    };
    if let Either::First(err) = select(sup.run_pools(&spawner), script).await {
        panic!("pool driver faulted on {}", err.node.name());
    }
}

#[test]
fn a_faulted_activate_still_grows_the_pool() {
    let clock = MockDriver::get();
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
        if ADVANCE.load(Ordering::SeqCst) {
            clock.advance(Duration::from_millis(10));
        }
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
