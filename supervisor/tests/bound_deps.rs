use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

supervisor_graph! {
    node LINK = Terminate, deps: [], task: link;

    node BOUND_A = Terminate, deps: [LINK ready bound], task: counted, slot_timeout: 5000;
    node BOUND_B = Terminate, deps: [BOUND_A ready bound], task: counted, slot_timeout: 5000;
    node PLAIN = Terminate, deps: [LINK ready], task: counted, slot_timeout: 5000;
}

static LINK_READY: AtomicBool = AtomicBool::new(true);
static STARTS_A: AtomicU32 = AtomicU32::new(0);
static STARTS_B: AtomicU32 = AtomicU32::new(0);
static STARTS_PLAIN: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn link(node: &'static TaskNode) {
    if LINK_READY.load(Ordering::SeqCst) {
        node.set_ready();
    }
    node.wait_shutdown().await;
    node.mark_exited();
}

/// Counts activations, and asserts readiness so the next hop can be bound to it.
async fn counted(node: &'static TaskNode) {
    let c = if core::ptr::eq(node, &BOUND_A) {
        &STARTS_A
    } else if core::ptr::eq(node, &BOUND_B) {
        &STARTS_B
    } else {
        &STARTS_PLAIN
    };
    c.fetch_add(1, Ordering::SeqCst);
    node.set_ready();
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn settle(sup: &Supervisor<4, GRAPH_TOPOLOGY>, spawner: Spawner) {
    sup.apply_bind(&spawner).await.expect("cascade");
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    sup.start(&spawner).await.expect("start");
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
    assert_eq!(
        BOUND_A.bound_deps().len(),
        1,
        "bound_a declares one bound dep"
    );
    assert_eq!(
        BOUND_B.bound_deps().len(),
        1,
        "bound_b declares one bound dep"
    );
    assert_eq!(PLAIN.bound_deps().len(), 0, "plain declares none");
    assert_eq!(STARTS_A.load(Ordering::SeqCst), 1);
    assert_eq!(STARTS_B.load(Ordering::SeqCst), 1);
    assert_eq!(STARTS_PLAIN.load(Ordering::SeqCst), 1);

    LINK.clear_ready();
    settle(&sup, spawner).await;

    assert!(!BOUND_A.is_running(), "a bound dependent is stopped");
    assert!(
        !BOUND_B.is_running(),
        "and the cascade is transitive along bound edges"
    );
    assert!(
        PLAIN.is_running(),
        "an unmarked `ready` edge keeps the status-not-control behaviour"
    );

    assert!(BOUND_A.is_bound_stopped());
    assert!(
        !BOUND_A.is_disabled(),
        "bound_stopped is distinct from disabled — it must lift by itself"
    );

    LINK.set_ready();
    settle(&sup, spawner).await;
    settle(&sup, spawner).await;

    assert_eq!(
        STARTS_A.load(Ordering::SeqCst),
        2,
        "the bound dependent came back"
    );
    assert_eq!(
        STARTS_B.load(Ordering::SeqCst),
        2,
        "and so did the second hop"
    );
    assert_eq!(
        STARTS_PLAIN.load(Ordering::SeqCst),
        1,
        "the plain dependent never moved"
    );
    assert!(!BOUND_A.is_bound_stopped(), "the flag lifted on recovery");

    sup.deactivate(&BOUND_A).await.expect("deactivate");
    assert!(BOUND_A.is_disabled());
    let before = STARTS_A.load(Ordering::SeqCst);

    LINK.clear_ready();
    settle(&sup, spawner).await;
    LINK.set_ready();
    settle(&sup, spawner).await;

    assert_eq!(
        STARTS_A.load(Ordering::SeqCst),
        before,
        "a node stopped on purpose is not restarted by a readiness flap"
    );
    assert!(BOUND_A.is_disabled(), "and stays disabled");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn readiness_propagates_across_bound_edges_only() {
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
        assert!(StdInstant::now() < deadline, "did not complete");
        clock.advance(embassy_time::Duration::from_millis(100));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
