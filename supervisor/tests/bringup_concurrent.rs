use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

#[derive(Clone, Copy)]
struct Handle {
    value: u32,
}

static SEEN: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn consumer_worker(node: &'static TaskNode, handle: Handle) {
    SEEN.store(handle.value, Ordering::SeqCst);
    DONE.store(true, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn provider_worker(node: &'static TaskNode) {
    HANDLE.provide(Handle { value: 7 });
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node CONSUMER = Terminate, deps: [], task: consumer_worker,
        slot_timeout: 5000,
        resources: [HANDLE: shared Handle];
    node PROVIDER = Terminate, deps: [], task: provider_worker;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner)
        .await
        .expect("the wave spawns the provider while the consumer's gate is pending");
}

#[test]
fn a_wave_reaches_a_provider_declared_after_its_consumer() {
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
            "bring-up did not resolve (seen={})",
            SEEN.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(1));
    }
    assert_eq!(SEEN.load(Ordering::SeqCst), 7);
    assert!(CONSUMER.is_running() && PROVIDER.is_running());
}
