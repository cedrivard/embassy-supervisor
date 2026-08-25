use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

supervisor_graph! {
    node ROOT = Terminate, deps: [], task: plain;

    node PROVIDER = Terminate, deps: [ROOT], task: provider;

    node CONSUMER = Terminate, deps: [PROVIDER ready], task: plain, slot_timeout: 5000;
    node DOWNSTREAM = Terminate, deps: [CONSUMER], task: plain;

    node SIBLING = Terminate, deps: [ROOT], task: plain, disabled;
}

static PROVIDER_GEN: AtomicU32 = AtomicU32::new(0);
static WITHHOLD_READY: AtomicBool = AtomicBool::new(false);
static CONSUMER_STARTS: AtomicU32 = AtomicU32::new(0);
static DOWNSTREAM_STARTS: AtomicU32 = AtomicU32::new(0);
static ROOT_STARTS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn plain(node: &'static TaskNode) {
    if core::ptr::eq(node, &CONSUMER) {
        CONSUMER_STARTS.fetch_add(1, Ordering::SeqCst);
    }
    if core::ptr::eq(node, &DOWNSTREAM) {
        DOWNSTREAM_STARTS.fetch_add(1, Ordering::SeqCst);
    }
    if core::ptr::eq(node, &ROOT) {
        ROOT_STARTS.fetch_add(1, Ordering::SeqCst);
    }
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn provider(node: &'static TaskNode) {
    PROVIDER_GEN.fetch_add(1, Ordering::SeqCst);
    if !WITHHOLD_READY.load(Ordering::SeqCst) {
        node.set_ready();
    }
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn settle() {
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    sup.start(&spawner).await.expect("start");
    settle().await;
    assert_eq!(PROVIDER_GEN.load(Ordering::SeqCst), 1);
    assert_eq!(CONSUMER_STARTS.load(Ordering::SeqCst), 1);
    assert_eq!(ROOT_STARTS.load(Ordering::SeqCst), 1);
    assert!(!SIBLING.is_running(), "declared `disabled`");

    sup.restart(&PROVIDER, &spawner).await.expect("restart");
    settle().await;

    assert_eq!(
        PROVIDER_GEN.load(Ordering::SeqCst),
        2,
        "the target was respawned"
    );
    assert_eq!(
        CONSUMER_STARTS.load(Ordering::SeqCst),
        2,
        "its dependent was cycled with it — the re-gate a bare stop/start misses"
    );
    assert!(DOWNSTREAM.is_running(), "the transitive dependent is back");
    assert_eq!(
        DOWNSTREAM_STARTS.load(Ordering::SeqCst),
        2,
        "back because the cascade cycled the second hop, not because it was \
         never stopped"
    );

    assert_eq!(
        ROOT_STARTS.load(Ordering::SeqCst),
        1,
        "a dependency of the target is not cycled"
    );
    assert!(
        !SIBLING.is_running() && SIBLING.is_disabled(),
        "a deliberately disabled node is not resurrected by an unrelated restart"
    );

    assert!(
        !PROVIDER.is_disabled() && !CONSUMER.is_disabled(),
        "a cycle is not a stop; nothing in the set is left marked disabled"
    );

    WITHHOLD_READY.store(true, Ordering::SeqCst);
    let before = CONSUMER_STARTS.load(Ordering::SeqCst);
    let err = sup
        .restart(&PROVIDER, &spawner)
        .await
        .expect_err("the consumer's ready gate must not pass");
    settle().await;
    assert!(
        matches!(
            err.kind,
            embassy_supervisor::FaultKind::ReadyDepTimeout { .. }
        ),
        "the gate that failed is named, not a generic spawn error: {err}"
    );
    assert_eq!(err.node.name(), "consumer", "the error names what failed");
    assert_eq!(
        CONSUMER_STARTS.load(Ordering::SeqCst),
        before,
        "the dependent did not start against an unready provider"
    );
    assert_eq!(
        PROVIDER_GEN.load(Ordering::SeqCst),
        3,
        "the target itself did come back"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn rest_for_one_cycles_downstream_only() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    // The last phase deliberately runs a gate to its timeout, so the mock clock
    // has to keep moving for the driver to get there.
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "did not complete");
        clock.advance(embassy_time::Duration::from_millis(100));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
