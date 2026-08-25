use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

static FINAL_REQ: AtomicBool = AtomicBool::new(false);
static FINAL_RESP: AtomicBool = AtomicBool::new(false);
static SERVICED: AtomicU32 = AtomicU32::new(0);
static RUNNER_STOPPED: AtomicBool = AtomicBool::new(false);
static ACKS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static DONE: AtomicBool = AtomicBool::new(false);

/// The dependency: answers requests only while not cancelled — an answered
/// request proves the node was still serving when it arrived.
async fn runner_worker(node: &'static TaskNode) {
    loop {
        let accepted = node
            .run_cancellable(async {
                while !FINAL_REQ.swap(false, Ordering::SeqCst) {
                    embassy_futures::yield_now().await;
                }
            })
            .await;
        if accepted.is_err() {
            break;
        }
        SERVICED.store(
            if node.shutdown_requested() { 2 } else { 1 },
            Ordering::SeqCst,
        );
        FINAL_RESP.store(true, Ordering::SeqCst);
    }
    ACKS.lock().unwrap().push("runner");
    RUNNER_STOPPED.store(true, Ordering::SeqCst);
    node.ack_dropped();
}

async fn ctrl_worker(node: &'static TaskNode) {
    node.wait_shutdown().await;
    FINAL_REQ.store(true, Ordering::SeqCst);
    while !FINAL_RESP.swap(false, Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    ACKS.lock().unwrap().push("ctrl");
    node.ack_dropped();
}

/// Unordered: its shutdown waits for `RUNNER` to be gone, which needs the
/// parked wave to release `RUNNER` on `CTRL`'s ack.
async fn watch_worker(node: &'static TaskNode) {
    node.wait_shutdown().await;
    while !RUNNER_STOPPED.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    ACKS.lock().unwrap().push("watch");
    node.ack_dropped();
}

supervisor_graph! {
    node RUNNER = Terminate, deps: [], task: runner_worker;
    node CTRL = Terminate, deps: [RUNNER], task: ctrl_worker;
    node WATCH = Terminate, deps: [], task: watch_worker;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    assert!(RUNNER.is_running() && CTRL.is_running() && WATCH.is_running());

    // Up-front signalling never returns from this call: RUNNER's service loop
    sup.teardown().await.expect("teardown completes");

    assert!(!RUNNER.is_running() && !CTRL.is_running() && !WATCH.is_running());
    assert_eq!(
        SERVICED.load(Ordering::SeqCst),
        1,
        "the final request was serviced before RUNNER was told to stop"
    );
    assert_eq!(*ACKS.lock().unwrap(), ["ctrl", "runner", "watch"]);
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_dependency_serves_until_its_dependents_have_acked() {
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
            "did not complete (serviced={}, acks={:?})",
            SERVICED.load(Ordering::SeqCst),
            ACKS.lock().unwrap(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
