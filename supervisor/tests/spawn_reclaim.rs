use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

static RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode, count: &mut u32) {
    *count += 1;
    RUNS.store(*count, Ordering::SeqCst);
    let _ = node.run_cancellable(core::future::pending::<()>()).await;
    // Arm the stale run-queue entry BEFORE the ack: the driver's wake is then
    core::future::poll_fn(|cx| {
        cx.waker().wake_by_ref();
        core::task::Poll::Ready(())
    })
    .await;
    node.ack_dropped();
}

supervisor_graph! {
    node A = Terminate, deps: [], task: worker,
        resources: [GADGET: u32];
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..100_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    GADGET.provide(0);

    sup.start(&spawner).await.expect("bring-up");
    settle(|| A.is_running()).await;
    sup.teardown().await.expect("clean stop");
    sup.start(&spawner)
        .await
        .expect("respawn must absorb the stale-entry Busy");
    settle(|| RUNS.load(Ordering::SeqCst) == 2).await;
    assert!(A.is_running() && RUNS.load(Ordering::SeqCst) == 2);

    sup.stop_node(&A).await.expect("clean stop");
    sup.start_node(&A, &spawner)
        .await
        .expect("start_node must absorb the stale-entry Busy");
    settle(|| RUNS.load(Ordering::SeqCst) == 3).await;
    assert!(A.is_running() && RUNS.load(Ordering::SeqCst) == 3);

    sup.stop_node(&A).await.expect("clean stop");
    sup.start_node(&A, &spawner).await.expect("probe passes");
    let stolen = GADGET.take().expect("shell has not polled yet");
    settle(|| !A.is_running()).await;
    assert_eq!(RUNS.load(Ordering::SeqCst), 3, "the body never ran");
    assert!(A.has_exited(), "read as a completed activation, not wedged");

    GADGET.provide(stolen);
    sup.start_node(&A, &spawner).await.expect("recovers");
    settle(|| RUNS.load(Ordering::SeqCst) == 4).await;
    assert!(A.is_running() && RUNS.load(Ordering::SeqCst) == 4);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_respawn_waits_out_the_previous_instances_storage_release() {
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
            "did not complete (runs={}, running={})",
            RUNS.load(Ordering::SeqCst),
            A.is_running(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
