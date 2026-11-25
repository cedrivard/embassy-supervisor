//! `task-priority` on the supervisor's own executor: a declared `priority:`
//! reaches the shell's task before its first poll, a higher priority is polled
//! first, `set_priority` is stored and taken by the node's own task at its next
//! start, and a plain verb leaves its caller's priority alone.

mod support;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use embassy_executor::{Metadata, Spawner};
use embassy_futures::join::join;
use embassy_supervisor::{PriorityError, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

type Go = Signal<CriticalSectionRawMutex, ()>;

supervisor_graph! {
    node LOW = Terminate, deps: [], priority: 1, task: worker(&crate::GO_LOW, 0);
    node HIGH = Terminate, deps: [], priority: 200, task: worker(&crate::GO_HIGH, 1);
    node PLAIN = Terminate, deps: [], task: worker(&crate::GO_PLAIN, 2);
    node HAND = Terminate, deps: [], spawn: hand;
}

static GO_LOW: Go = Signal::new();
static GO_HIGH: Go = Signal::new();
static GO_PLAIN: Go = Signal::new();
/// Each worker's priority as its task saw it when the activation began.
static SEEN: [AtomicU8; 3] = [AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0)];
static STARTS: AtomicUsize = AtomicUsize::new(0);
/// Worker tags in the order their wakes were polled.
static ORDER: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Worker tags in the order of their activations' first polls.
static FIRST: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode, go: &'static Go, tag: usize) {
    FIRST.lock().unwrap().push(tag);
    SEEN[tag].store(
        Metadata::for_current_task().await.priority(),
        Ordering::SeqCst,
    );
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

#[embassy_executor::task]
async fn hand(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

fn seen() -> [u8; 3] {
    SEEN.each_ref().map(|s| s.load(Ordering::SeqCst))
}

fn order() -> Vec<usize> {
    ORDER.lock().unwrap().clone()
}

async fn own_priority() -> u8 {
    Metadata::for_current_task().await.priority()
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    assert_eq!(LOW.priority(), Some(1));
    assert_eq!(PLAIN.priority(), Some(0));
    assert_eq!(HAND.priority(), None);
    assert_eq!(HAND.set_priority(9), Err(PriorityError::NoShell));
    // Before the first start: the spawn stamp takes the value.
    assert_eq!(LOW.set_priority(3), Ok(()));
    assert_eq!(GRAPH.supervisor_priority, 255);

    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("boot");
    assert_eq!(own_priority().await, 0, "start left the caller alone");
    support::settle("three shells started", || {
        STARTS.load(Ordering::SeqCst) == 3
    })
    .await;
    assert_eq!(seen(), [3, 200, 0]);
    // Stamped at spawn, so the first polls already go by priority. Unstamped
    // tasks would all start at 0 and be polled in reverse spawn order.
    assert_eq!(*FIRST.lock().unwrap(), [1, 0, 2]);

    // One batch of wakes is polled by priority, not by wake order.
    GO_HIGH.signal(());
    GO_LOW.signal(());
    GO_PLAIN.signal(());
    support::settle("three wakes", || order().len() == 3).await;
    assert_eq!(order(), [1, 0, 2]);

    // `set_priority` is stored: the running activation keeps its priority.
    assert_eq!(PLAIN.set_priority(250), Ok(()));
    assert_eq!(PLAIN.priority(), Some(250));
    ORDER.lock().unwrap().clear();
    GO_HIGH.signal(());
    GO_PLAIN.signal(());
    support::settle("two wakes", || order().len() == 2).await;
    assert_eq!(order(), [1, 2]);

    // A plain verb leaves its caller's priority alone, and code joined with it too.
    let (res, during) = join(sup.stop_node(&PLAIN), own_priority()).await;
    res.expect("PLAIN acks its stop");
    assert_eq!(during, 0);
    assert!(!PLAIN.is_running());

    // The node's own task takes the value at its next start.
    sup.start_node(&PLAIN, &spawner).await.expect("restart");
    support::settle("PLAIN restarted", || STARTS.load(Ordering::SeqCst) == 4).await;
    assert_eq!(seen()[2], 250);
    ORDER.lock().unwrap().clear();
    GO_PLAIN.signal(());
    GO_HIGH.signal(());
    support::settle("two wakes", || order().len() == 2).await;
    assert_eq!(order(), [2, 1]);
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn priorities_order_polls_and_follow_set_priority() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "starts {}, seen {:?}, order {:?}",
                STARTS.load(Ordering::SeqCst),
                seen(),
                order()
            )
        },
    );
}
