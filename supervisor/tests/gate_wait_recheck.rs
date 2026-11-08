//! The supervisor's gate waits watch the shared gate event, not a slot's
//! single-waker `Signal`: a thief that takes the fill's wake on that `Signal`
//! does not delay the node's start.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{ResourceGate, Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver};

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

/// Fills the slot and takes the fill's wake in the same poll, as a second
/// waiter parked on the same latch would.
#[embassy_executor::task]
async fn thief() {
    for _ in 0..16 {
        embassy_futures::yield_now().await;
    }
    HANDLE.provide(7);
    ResourceGate::filled_signal(&HANDLE).reset();
    STOLEN.store(true, Ordering::SeqCst);
}

#[test]
fn a_timed_out_resource_wait_rechecks_the_slot() {
    let clock = MockDriver::get();
    let thread = std::thread::spawn(|| {
        let executor = Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(starter(spawner).unwrap());
            spawner.spawn(thief().unwrap());
        });
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        if thread.is_finished() {
            thread.join().expect("executor assertions failed");
            panic!("executor exited before completion");
        }
        assert!(StdInstant::now() < deadline, "did not complete");
        if STOLEN.load(Ordering::SeqCst) {
            clock.advance(Duration::from_millis(50));
        }
        std::thread::sleep(StdDuration::from_millis(2));
    }
    assert_eq!(SEEN.load(Ordering::SeqCst), 7);
}
