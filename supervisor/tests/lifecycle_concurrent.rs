use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Leased, Supervisor, TaskNode, dataflow, supervisor_graph};

pub static HANDLE: Leased<AtomicU32> = Leased::new(AtomicU32::new(0));

static ACKS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static CONS_HOLDS: AtomicBool = AtomicBool::new(false);
/// Pins the interleaving (see `teardown_concurrent.rs`): the consumer lets go
/// only once the producer is provably parked in its drain.
static PROD_DRAINING: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn root(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.ack_dropped();
}

#[dataflow]
async fn consumer(node: &'static TaskNode) {
    // The producer reopens on its way up, and `restart` spawns this node
    // first (topological order), so wait out the window where the signal is
    // still closed from the previous cycle's drain.
    let held = loop {
        if let Some(h) = node.lease(&crate::HANDLE) {
            break h;
        }
        embassy_futures::yield_now().await;
    };
    CONS_HOLDS.store(true, Ordering::SeqCst);
    node.wait_shutdown().await;
    while !PROD_DRAINING.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    drop(held);
    ACKS.lock().unwrap().push("cons");
    node.ack_dropped();
}

#[dataflow]
async fn producer(node: &'static TaskNode) {
    HANDLE.reopen();
    node.writer(&crate::HANDLE).store(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    PROD_DRAINING.store(true, Ordering::SeqCst);
    HANDLE.drain().await;
    ACKS.lock().unwrap().push("prod");
    node.ack_dropped();
}

// `CONS` and `PROD` are unordered between themselves, `PROD` last, so reverse
// topological order reaches the producer before the consumer it waits on.
supervisor_graph! {
    node ROOT = Terminate, deps: [], task: root;
    node CONS = Terminate, deps: [ROOT], task: consumer, discover;
    node PROD = Terminate, deps: [ROOT], task: producer, discover;
}

/// One stopped pass just completed: both leaves down, the lease released, and
/// the consumer's ack collected before the producer finished draining.
fn assert_pass_down() {
    assert!(!CONS.is_running() && !PROD.is_running(), "leaves are down");
    assert_eq!(HANDLE.leases(), 0, "the lease was released, not leaked");
    assert_eq!(
        *ACKS.lock().unwrap(),
        ["cons", "prod"],
        "the consumer let go before the producer finished draining"
    );
}

fn arm_pass() {
    ACKS.lock().unwrap().clear();
    CONS_HOLDS.store(false, Ordering::SeqCst);
    PROD_DRAINING.store(false, Ordering::SeqCst);
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    while !CONS_HOLDS.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    PHASE.store(1, Ordering::SeqCst);

    sup.deactivate(&ROOT).await.expect("deactivate completes");
    assert_pass_down();
    assert!(!ROOT.is_running() && ROOT.is_disabled(), "the stop sticks");
    PHASE.store(2, Ordering::SeqCst);

    arm_pass();
    sup.activate(&CONS, &spawner).await;
    sup.activate(&PROD, &spawner).await;
    while !CONS_HOLDS.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    PHASE.store(3, Ordering::SeqCst);

    arm_pass();
    sup.restart(&ROOT, &spawner)
        .await
        .expect("restart completes");
    assert_eq!(
        *ACKS.lock().unwrap(),
        ["cons", "prod"],
        "restart's down half collected the same ack order"
    );
    while !CONS_HOLDS.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    assert!(
        ROOT.is_running() && CONS.is_running() && PROD.is_running(),
        "restart brought the set back"
    );
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn cascading_stops_signal_before_they_wait() {
    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (phase={}, leases={}, acks={:?})",
            PHASE.load(Ordering::SeqCst),
            HANDLE.leases(),
            ACKS.lock().unwrap(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
