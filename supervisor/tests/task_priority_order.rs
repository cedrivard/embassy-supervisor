//! `task-priority`: `set_priority` from another task only stores the value. A
//! task that already waits in the executor's sorted pending list keeps its
//! priority, so tasks queued after it are still sorted correctly.

mod support;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

type Go = Signal<CriticalSectionRawMutex, ()>;

supervisor_graph! {
    node X = Terminate, deps: [], priority: 250, task: x;
    node A = Terminate, deps: [], priority: 200, task: worker(&crate::GO_A, 'A');
    node B = Terminate, deps: [], priority: 100, task: worker(&crate::GO_B, 'B');
    node C = Terminate, deps: [], priority: 10, task: worker(&crate::GO_C, 'C');
}

static GO_X: Go = Signal::new();
static GO_A: Go = Signal::new();
static GO_B: Go = Signal::new();
static GO_C: Go = Signal::new();
static STARTS: AtomicUsize = AtomicUsize::new(0);
static ORDER: Mutex<Vec<char>> = Mutex::new(Vec::new());
static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode, go: &'static Go, tag: char) {
    STARTS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(async {
            loop {
                go.wait().await;
                ORDER.lock().unwrap().push(tag);
            }
        })
        .await;
}

/// Polled first of its batch. A and B already sit in the sorted list then.
async fn x(node: &'static TaskNode) {
    STARTS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(async {
            loop {
                GO_X.wait().await;
                ORDER.lock().unwrap().push('X');
                A.set_priority(0).unwrap();
                GO_C.signal(());
            }
        })
        .await;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("boot");
    support::settle("four shells started", || STARTS.load(Ordering::SeqCst) == 4).await;
    GO_X.signal(());
    GO_A.signal(());
    GO_B.signal(());
    support::settle("four wakes", || ORDER.lock().unwrap().len() == 4).await;
    // A keeps 200 while it is queued: X(250), A(200), B(100), then C(10).
    assert_eq!(*ORDER.lock().unwrap(), ['X', 'A', 'B', 'C']);
    assert_eq!(A.priority(), Some(0));
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_queued_task_keeps_its_priority_until_its_next_start() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || format!("order {:?}", ORDER.lock().unwrap()),
    );
}
