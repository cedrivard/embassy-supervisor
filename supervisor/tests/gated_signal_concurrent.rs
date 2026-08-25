use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Backed, Supervisor, TaskNode, dataflow, supervisor_graph};

pub static ESTIMATE: Backed<AtomicU32> = Backed::new(AtomicU32::new(0));

static PRODUCER_SPAWNS: AtomicU32 = AtomicU32::new(0);
static FIRST_SAW: AtomicU32 = AtomicU32::new(0);
static SECOND_SAW: AtomicU32 = AtomicU32::new(0);

supervisor_graph! {
    node ESTIMATOR = Terminate, deps: [], task: estimator, disabled, discover;
    node FIRST = Terminate, deps: [], task: first_consumer, discover;
    node SECOND = Terminate, deps: [], task: second_consumer, discover;
}

#[dataflow]
async fn estimator(node: &'static TaskNode) {
    PRODUCER_SPAWNS.fetch_add(1, Ordering::SeqCst);
    for _ in 0..64 {
        embassy_futures::yield_now().await;
    }
    node.writer(&crate::ESTIMATE).store(42, Ordering::SeqCst);
    node.set_ready();
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

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
    let est = node.open(&crate::ESTIMATE).await;
    SECOND_SAW.store(est.load(Ordering::SeqCst), Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    let fault = sup.run(&spawner).await;
    panic!("driver returned: {fault}");
}

#[test]
fn concurrent_openers_both_pass_one_start() {
    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(runner(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while FIRST_SAW.load(Ordering::SeqCst) == 0 || SECOND_SAW.load(Ordering::SeqCst) == 0 {
        assert!(
            StdInstant::now() < deadline,
            "an opener is parked (spawns={}, first={}, second={})",
            PRODUCER_SPAWNS.load(Ordering::SeqCst),
            FIRST_SAW.load(Ordering::SeqCst),
            SECOND_SAW.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }

    assert_eq!(PRODUCER_SPAWNS.load(Ordering::SeqCst), 1, "one start");
    assert_eq!(FIRST_SAW.load(Ordering::SeqCst), 42);
    assert_eq!(SECOND_SAW.load(Ordering::SeqCst), 42);
}
