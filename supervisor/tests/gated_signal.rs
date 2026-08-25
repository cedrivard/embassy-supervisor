use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    Backed, ControlOp, Supervisor, TaskNode, dataflow, producer_of, supervisor_graph,
    try_request_control,
};

pub static ESTIMATE: Backed<AtomicU32> = Backed::new(AtomicU32::new(0));

static PRODUCER_SPAWNS: AtomicU32 = AtomicU32::new(0);
static FIRST_SAW: AtomicU32 = AtomicU32::new(0);
static SECOND_SAW: AtomicU32 = AtomicU32::new(0);
static THIRD_SAW: AtomicU32 = AtomicU32::new(0);
static PRODUCER_UP_AT_START: AtomicBool = AtomicBool::new(false);
static SECOND_CYCLE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

supervisor_graph! {
    node ESTIMATOR = Terminate, deps: [], task: estimator, disabled, discover;
    node FIRST = Terminate, deps: [], task: first_consumer, discover;
    node SECOND = Terminate, deps: [], task: second_consumer, discover;
    node THIRD = Terminate, deps: [], task: third_consumer, discover;
}

#[dataflow]
async fn estimator(node: &'static TaskNode) {
    PRODUCER_SPAWNS.fetch_add(1, Ordering::SeqCst);
    node.writer(&crate::ESTIMATE).store(42, Ordering::SeqCst);
    node.set_ready();
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// The first consumer: its read is what starts the producer.
#[dataflow]
async fn first_consumer(node: &'static TaskNode) {
    let est = node.open(&crate::ESTIMATE).await;
    FIRST_SAW.store(est.load(Ordering::SeqCst), Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

#[dataflow]
async fn second_consumer(node: &'static TaskNode) {
    while FIRST_SAW.load(Ordering::SeqCst) == 0 {
        embassy_futures::yield_now().await;
    }
    let est = node.open(&crate::ESTIMATE).await;
    SECOND_SAW.store(est.load(Ordering::SeqCst), Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// The third consumer: opens only after the producer has been stopped again,
/// so its `open` is the second cycle's first — the one a lifetime latch would
#[dataflow]
async fn third_consumer(node: &'static TaskNode) {
    while !SECOND_CYCLE.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    let est = node.open(&crate::ESTIMATE).await;
    THIRD_SAW.store(est.load(Ordering::SeqCst), Ordering::SeqCst);
    DONE.store(true, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// The whole app: one `run()`. Nothing wires the signal to its producer.
#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    PRODUCER_UP_AT_START.store(ESTIMATOR.is_running(), Ordering::SeqCst);

    // The driver loop is what turns the gate's control request into a spawn.
    let fault = sup.run(&spawner).await;
    panic!("driver returned: {fault}");
}

#[test]
fn open_starts_the_producer_and_waits_for_readiness() {
    assert!(core::ptr::eq(
        producer_of(&FIRST, &FIRST.reads()[0][0]).expect("the graph knows its writer"),
        &ESTIMATOR
    ));

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(runner(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    let wait_for = |what: &str, cond: &dyn Fn() -> bool| {
        while !cond() {
            assert!(
                StdInstant::now() < deadline,
                "{what} did not resolve (spawns={}, first={})",
                PRODUCER_SPAWNS.load(Ordering::SeqCst),
                FIRST_SAW.load(Ordering::SeqCst),
            );
            std::thread::sleep(StdDuration::from_millis(5));
        }
    };
    wait_for("first cycle", &|| SECOND_SAW.load(Ordering::SeqCst) != 0);

    assert!(
        !PRODUCER_UP_AT_START.load(Ordering::SeqCst),
        "the producer is down after start(): only an `open` brings it up"
    );
    assert_eq!(
        PRODUCER_SPAWNS.load(Ordering::SeqCst),
        1,
        "two openers, one start"
    );
    assert_eq!(
        FIRST_SAW.load(Ordering::SeqCst),
        42,
        "`open` returned only after the producer published and set_ready"
    );
    assert_eq!(SECOND_SAW.load(Ordering::SeqCst), 42, "late opener passes");

    // ── the latch is per down cycle ─────────────────────────────────────
    // Stop the producer again through the same mailbox the gate uses; once it
    // is down, the third opener's `open` must enqueue a fresh start rather
    try_request_control(&ESTIMATOR, ControlOp::Deactivate).expect("mailbox has room");
    wait_for("the producer's stop", &|| !ESTIMATOR.is_running());
    SECOND_CYCLE.store(true, Ordering::SeqCst);
    wait_for("second cycle", &|| DONE.load(Ordering::SeqCst));

    assert_eq!(
        PRODUCER_SPAWNS.load(Ordering::SeqCst),
        2,
        "the second cycle's open re-started the producer"
    );
    assert_eq!(
        THIRD_SAW.load(Ordering::SeqCst),
        42,
        "against a fresh instance"
    );

    let reads: Vec<&str> = FIRST
        .reads()
        .iter()
        .flat_map(|t| t.iter())
        .map(|c| c.name())
        .collect();
    assert_eq!(reads, ["crate::ESTIMATE"]);
    assert!(
        SECOND.writes().iter().all(|t| t.is_empty()),
        "a gate is a read, never a write"
    );
}
