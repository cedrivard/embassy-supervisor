use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node POLICED = Terminate, deps: [], task: worker, beat_timeout: 100;
    node FREE = Terminate, deps: [], task: worker;
}

static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn settle() {
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

fn stale() -> Vec<&'static str> {
    GRAPH.stale_nodes().map(TaskNode::name).collect()
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    let sup = Supervisor::new(&GRAPH);

    sup.start(&spawner).await.expect("start");
    settle().await;
    assert!(GRAPH.all_fresh(), "the spawn stamps a beat");

    clock.advance(Duration::from_millis(150));
    assert_eq!(
        stale(),
        ["policed"],
        "past its budget the policed node withholds the feed; FREE never does"
    );
    assert!(!GRAPH.all_fresh());

    POLICED.beat();
    assert!(GRAPH.all_fresh(), "a beat since the last look is granted");
    clock.advance(Duration::from_millis(60));
    assert!(GRAPH.all_fresh(), "and restarts the budget");

    clock.advance(Duration::from_millis(60));
    assert_eq!(stale(), ["policed"]);
    sup.stop_node(&POLICED).await.expect("stop");
    assert!(
        GRAPH.all_fresh(),
        "a stopped node never withholds the feed, however long since its beat"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn all_fresh_gates_on_policed_running_nodes_only() {
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
