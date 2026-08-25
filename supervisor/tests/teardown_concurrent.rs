use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Leased, Supervisor, TaskNode, dataflow, supervisor_graph};

pub static HANDLE: Leased<AtomicU32> = Leased::new(AtomicU32::new(1));

static ACKS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static CONS_HOLDS: AtomicBool = AtomicBool::new(false);
/// Set by `PROD` once it is about to drain, and waited on by `CONS` before it
/// lets go. That pins the interleaving the test is about — the producer parks
/// in `drain` with a live lease and is woken by the drop — instead of leaving
/// it to whichever task the executor happens to poll first.
static PROD_DRAINING: AtomicBool = AtomicBool::new(false);
/// Set if `PROD` saw a live lease when it reached its drain.
static DRAIN_HAD_TO_WAIT: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// The consumer: takes a lease, holds it across its whole run, and drops it
/// only when it is told to stop.
#[dataflow]
async fn consumer(node: &'static TaskNode) {
    let held = node.lease(&crate::HANDLE).expect("open at bring-up");
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
    node.writer(&crate::HANDLE).store(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    DRAIN_HAD_TO_WAIT.store(HANDLE.leases() > 0, Ordering::SeqCst);
    PROD_DRAINING.store(true, Ordering::SeqCst);
    HANDLE.drain().await;
    ACKS.lock().unwrap().push("prod");
    node.ack_dropped();
}

// Unordered on purpose, and `PROD` last so reverse topological order reaches
// it before `CONS`.
supervisor_graph! {
    node CONS = Terminate, deps: [], task: consumer, discover;
    node PROD = Terminate, deps: [], task: producer, discover;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    PHASE.store(1, Ordering::SeqCst);

    // Give the consumer a chance to take its lease before anything stops.
    while !CONS_HOLDS.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    assert_eq!(HANDLE.leases(), 1, "the consumer is holding");
    PHASE.store(2, Ordering::SeqCst);

    // Sequential signalling never returns from this call.
    sup.teardown().await.expect("teardown completes");
    PHASE.store(3, Ordering::SeqCst);

    assert!(!CONS.is_running() && !PROD.is_running(), "both marked down");
    assert_eq!(HANDLE.leases(), 0, "the lease was released, not leaked");
    assert!(
        DRAIN_HAD_TO_WAIT.load(Ordering::SeqCst),
        "the producer reached its drain with the lease still live"
    );
    // Both were signalled in the wave's first scan; the producer's ack is
    // nonetheless the last to arrive, because it waited for the consumer.
    assert_eq!(
        *ACKS.lock().unwrap(),
        ["cons", "prod"],
        "the consumer let go before the producer finished draining"
    );
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_producer_may_wait_on_a_consumer_the_supervisor_has_not_acked() {
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
            "did not complete (phase={}, leases={}, acks={:?}) — a teardown that \
             signals one node at a time deadlocks here",
            PHASE.load(Ordering::SeqCst),
            HANDLE.leases(),
            ACKS.lock().unwrap(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
