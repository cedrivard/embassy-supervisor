use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

supervisor_graph! {
    node LINK = Terminate, task: link;

    node BOUND = Terminate, deps: [LINK ready bound], task: counted, slot_timeout: 300;
    // A plain dep on the parked node: must spawn once BOUND leaves the wave,
    // not hang behind it.
    node CHILD = Terminate, deps: [BOUND], task: counted;
}

static LINK_READY: AtomicBool = AtomicBool::new(false);
static STARTS_BOUND: AtomicU32 = AtomicU32::new(0);
static STARTS_CHILD: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn link(node: &'static TaskNode) {
    if LINK_READY.load(Ordering::SeqCst) {
        node.set_ready();
    }
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn counted(node: &'static TaskNode) {
    let c = if core::ptr::eq(node, &BOUND) {
        &STARTS_BOUND
    } else {
        &STARTS_CHILD
    };
    c.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn settle(sup: &Supervisor<3, GRAPH_TOPOLOGY>, spawner: Spawner) {
    sup.apply_bind(&spawner).await.expect("cascade");
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    // The provider is not ready at boot: the wave must complete without a
    // fault, parking the bound dependent and spawning the plain one.
    sup.start(&spawner)
        .await
        .expect("a bound edge defers, not faults");
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
    assert_eq!(
        STARTS_BOUND.load(Ordering::SeqCst),
        0,
        "parked, not spawned"
    );
    assert!(BOUND.is_bound_stopped(), "parked as a bound stop");
    assert!(!BOUND.is_disabled(), "not a manual stop");
    assert_eq!(
        STARTS_CHILD.load(Ordering::SeqCst),
        1,
        "a plain dependent of the parked node still spawns"
    );

    // The provider asserts: the bind loop lifts the park.
    LINK_READY.store(true, Ordering::SeqCst);
    LINK.set_ready();
    settle(&sup, spawner).await;

    assert_eq!(
        STARTS_BOUND.load(Ordering::SeqCst),
        1,
        "lifted on set_ready"
    );
    assert!(BOUND.is_running());
    assert!(!BOUND.is_bound_stopped(), "the flag lifted with the spawn");

    // An Activate while the provider is down parks again instead of faulting.
    LINK.clear_ready();
    settle(&sup, spawner).await;
    assert!(!BOUND.is_running(), "bound-stopped by the withdrawal");
    sup.start_node(&BOUND, &spawner)
        .await
        .expect("a direct start over an un-ready bound dep parks, not faults");
    assert!(!BOUND.is_running());
    assert!(BOUND.is_bound_stopped());

    LINK.set_ready();
    settle(&sup, spawner).await;
    assert_eq!(STARTS_BOUND.load(Ordering::SeqCst), 2, "and lifts again");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_bound_edge_defers_bring_up_instead_of_faulting() {
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
