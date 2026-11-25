//! `task-priority`: a `Pause` node never leaves its activation, so its task
//! takes a `set_priority` value when it resumes. Both resume paths:
//! `wait_resume()` and `run_pausable`.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use embassy_executor::{Metadata, Spawner};
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

supervisor_graph! {
    node MANUAL = Pause, deps: [], priority: 4, task: manual;
    node LOOPED = Pause, deps: [], priority: 4, task: looped;
}

/// Each worker's priority as its task saw it when it last started or resumed.
static SEEN: [AtomicU8; 2] = [AtomicU8::new(0), AtomicU8::new(0)];
static RUNS: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
static DONE: AtomicBool = AtomicBool::new(false);

async fn record(i: usize) {
    let p = Metadata::for_current_task().await.priority();
    SEEN[i].store(p, Ordering::SeqCst);
    RUNS[i].fetch_add(1, Ordering::SeqCst);
}

async fn manual(node: &'static TaskNode) {
    loop {
        record(0).await;
        if node
            .run_cancellable(core::future::pending::<()>())
            .await
            .is_err()
        {
            node.ack_dropped();
            node.wait_resume().await;
        }
    }
}

async fn looped(node: &'static TaskNode) {
    node.run_pausable_loop(async || {
        record(1).await;
        core::future::pending::<()>().await
    })
    .await
}

fn seen() -> [u8; 2] {
    SEEN.each_ref().map(|s| s.load(Ordering::SeqCst))
}

fn runs() -> [usize; 2] {
    RUNS.each_ref().map(|r| r.load(Ordering::SeqCst))
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("boot");
    support::settle("both started", || runs() == [1, 1]).await;
    assert_eq!(seen(), [4, 4]);

    assert_eq!(MANUAL.set_priority(9), Ok(()));
    assert_eq!(LOOPED.set_priority(9), Ok(()));
    sup.teardown().await.expect("both pause");
    assert_eq!(seen(), [4, 4]);
    sup.start(&spawner).await.expect("both resume");
    support::settle("both resumed", || runs() == [2, 2]).await;
    assert_eq!(seen(), [9, 9]);
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_pause_node_takes_its_priority_when_it_resumes() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || format!("runs {:?}, seen {:?}", runs(), seen()),
    );
}
