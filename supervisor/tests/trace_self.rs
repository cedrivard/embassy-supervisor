use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph};

supervisor_graph! {
    node WORKER = Pause, deps: [];
}

static DONE: AtomicBool = AtomicBool::new(false);

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..100_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn worker_body() {
    WORKER.adopt_current().await;
    core::future::pending::<()>().await;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");

    let node = GRAPH.graph_ref.self_node().expect("hidden self-node");
    assert_eq!(node.name(), "supervisor");
    assert_ne!(node.task_id(), 0, "start() stamped the calling task's id");
    assert!(node.is_running() && node.is_detached());

    let seen = node.poll_count();
    settle(|| node.poll_count() > seen).await;
    assert!(node.poll_count() > seen, "host-task polls attributed");

    assert_eq!(WORKER.task_id(), 0, "parked: nothing auto-mapped");
    spawner.spawn(worker_body().unwrap());
    settle(|| WORKER.poll_count() > 0).await;
    assert_ne!(WORKER.task_id(), 0, "the body registered itself");
    assert!(WORKER.poll_count() > 0, "and its polls are attributed");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn start_adopts_its_host_task_into_the_self_node() {
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
            "did not complete (self_id={}, worker_id={})",
            GRAPH.graph_ref.self_node().map_or(0, |n| n.task_id()),
            WORKER.task_id(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
