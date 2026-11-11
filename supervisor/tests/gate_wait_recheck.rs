//! The supervisor's gate wait re-checks the slot on the shared gate event: a
//! fill that lands during the wait starts the node.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

static SEEN: AtomicU32 = AtomicU32::new(0);
static STOLEN: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn consumer(node: &'static TaskNode, handle: u32) {
    SEEN.store(handle, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node CONSUMER = Terminate, deps: [], task: consumer, disabled,
        resources: [HANDLE: shared u32], slot_timeout: 200;
}

#[embassy_executor::task]
async fn starter(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner)
        .await
        .expect("a disabled node is skipped");
    sup.start_node(&CONSUMER, &spawner)
        .await
        .expect("filled before the deadline: started, whoever took the wake");
    assert!(CONSUMER.is_running());
    DONE.store(true, Ordering::SeqCst);
}

/// Fills the slot late, from another task.
#[embassy_executor::task]
async fn thief() {
    for _ in 0..16 {
        embassy_futures::yield_now().await;
    }
    HANDLE.provide(7);
    STOLEN.store(true, Ordering::SeqCst);
}

#[test]
fn a_timed_out_resource_wait_rechecks_the_slot() {
    support::run(
        |spawner| {
            spawner.spawn(starter(spawner).unwrap());
            spawner.spawn(thief().unwrap());
        },
        &DONE,
        support::Clock::While(&STOLEN, embassy_time::Duration::from_millis(50)),
        10,
        || "did not complete".to_owned(),
    );
    assert_eq!(SEEN.load(Ordering::SeqCst), 7);
}
