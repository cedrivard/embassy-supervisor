//! One driver loop per supervisor: a bound stop and a Restart aimed at the
//! same node run one after the other, never as two waves at once, so neither
//! times out the instance the other respawned.

mod support;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{ControlOp, Supervisor, TaskNode, supervisor_graph, try_request_control};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node LINK = Terminate, deps: [], task: link;
    node SESSION = Terminate, deps: [LINK ready bound], task: session, slot_timeout: 5000;
}

static LINK_STARTS: AtomicU32 = AtomicU32::new(0);
static SESSION_STARTS: AtomicU32 = AtomicU32::new(0);
static ACK_GO: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static FAULT: Mutex<Option<String>> = Mutex::new(None);

async fn until(cond: impl Fn() -> bool) {
    while !cond() {
        embassy_futures::yield_now().await;
    }
}

async fn link(node: &'static TaskNode) {
    LINK_STARTS.fetch_add(1, Ordering::SeqCst);
    node.set_ready();
    node.wait_shutdown().await;
    node.mark_exited();
}

/// The first instance holds its ack until the script releases it, so both
/// requests are pending while SESSION is being stopped.
async fn session(node: &'static TaskNode) {
    let n = SESSION_STARTS.fetch_add(1, Ordering::SeqCst) + 1;
    node.wait_shutdown().await;
    if n == 1 {
        until(|| ACK_GO.load(Ordering::SeqCst)).await;
    }
    node.mark_exited();
}

async fn script() {
    let clock = MockDriver::get();
    until(|| SESSION.is_running() && LINK.is_ready()).await;
    LINK.clear_ready();
    try_request_control(&LINK, ControlOp::Restart).expect("mailbox has room");
    for _ in 0..64 {
        embassy_futures::yield_now().await;
    }
    ACK_GO.store(true, Ordering::SeqCst);
    until(|| SESSION_STARTS.load(Ordering::SeqCst) == 2 && SESSION.is_running() && LINK.is_ready())
        .await;
    // Past every ack deadline a second wave on SESSION would have armed.
    for _ in 0..30 {
        clock.advance(Duration::from_millis(100));
        for _ in 0..8 {
            embassy_futures::yield_now().await;
        }
    }
    assert_eq!(LINK_STARTS.load(Ordering::SeqCst), 2, "LINK restarted once");
    assert_eq!(
        SESSION_STARTS.load(Ordering::SeqCst),
        2,
        "SESSION came back once"
    );
    assert!(SESSION.is_running() && !SESSION.is_bound_stopped());
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    if let Either::First(fault) = select(sup.run(&spawner), script()).await {
        *FAULT.lock().unwrap() = Some(fault.to_string());
    }
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_bind_and_a_restart_on_one_node_never_overlap() {
    // Frozen mock clock: the script advances it itself.
    support::run(
        |spawner| spawner.spawn(runner(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
    assert_eq!(*FAULT.lock().unwrap(), None, "run() returned a fault");
}
