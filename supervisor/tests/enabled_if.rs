use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

static FLAG: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

fn flag() -> bool {
    CALLS.fetch_add(1, Ordering::SeqCst);
    FLAG.load(Ordering::SeqCst)
}

supervisor_graph! {
    node GATED = Terminate, deps: [], task: worker, enabled_if: flag;
    node ALWAYS = Terminate, deps: [], task: worker;
}

async fn worker(node: &'static TaskNode) {
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
    assert!(ALWAYS.is_running());
    assert!(!GATED.is_running(), "a false predicate keeps the node down");
    assert!(GATED.is_disabled(), "and latches it disabled, visibly");
    assert_eq!(CALLS.load(Ordering::SeqCst), 1, "evaluated once per start");

    sup.activate(&GATED, &spawner)
        .await
        .expect("activate overrides the latch");
    settle().await;
    assert!(GATED.is_running() && !GATED.is_disabled());

    sup.start(&spawner).await.expect("re-entry");
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "a running node is not re-evaluated, so re-entry cannot latch it"
    );
    assert!(GATED.is_running() && !GATED.is_disabled());

    sup.teardown().await.expect("teardown");
    FLAG.store(true, Ordering::SeqCst);
    sup.start(&spawner).await.expect("start");
    settle().await;
    assert!(GATED.is_running(), "a true predicate brings it up");

    sup.teardown().await.expect("teardown");
    GATED.set_disabled(true);
    sup.start(&spawner).await.expect("start");
    settle().await;
    assert!(
        !GATED.is_running() && GATED.is_disabled(),
        "a true predicate never clears a manual latch"
    );
    assert!(ALWAYS.is_running());

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn enabled_if_latches_disabled_at_start() {
    let _clock = MockDriver::get();

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
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
