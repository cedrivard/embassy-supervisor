mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use support::settle;

static RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode, count: &mut u32) {
    *count += 1;
    RUNS.store(*count, Ordering::SeqCst);
    let _ = node.run_cancellable(core::future::pending::<()>()).await;
    // Arm the stale run-queue entry BEFORE the ack: the driver's wake is then
    // enqueued after it, and a batch runs in reverse enqueue order
    // (embassy-executor run_queue.rs), so the respawn meets this storage still
    // queued: `Busy` for one pass.
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

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    GADGET.provide(0);

    sup.start(&spawner).await.expect("bring-up");
    settle("A running", || A.is_running()).await;
    sup.teardown().await.expect("clean stop");
    sup.start(&spawner)
        .await
        .expect("respawn must absorb the stale-entry Busy");
    settle("second run", || RUNS.load(Ordering::SeqCst) == 2).await;
    assert!(A.is_running() && RUNS.load(Ordering::SeqCst) == 2);

    sup.stop_node(&A).await.expect("clean stop");
    sup.start_node(&A, &spawner)
        .await
        .expect("start_node must absorb the stale-entry Busy");
    settle("third run", || RUNS.load(Ordering::SeqCst) == 3).await;
    assert!(A.is_running() && RUNS.load(Ordering::SeqCst) == 3);

    sup.stop_node(&A).await.expect("clean stop");
    sup.start_node(&A, &spawner).await.expect("probe passes");
    let stolen = GADGET.take().expect("shell has not polled yet");
    settle("A down without its resource", || !A.is_running()).await;
    assert_eq!(RUNS.load(Ordering::SeqCst), 3, "the body never ran");
    assert!(A.has_exited(), "read as a completed activation, not wedged");

    GADGET.provide(stolen);
    sup.start_node(&A, &spawner).await.expect("recovers");
    settle("fourth run", || RUNS.load(Ordering::SeqCst) == 4).await;
    assert!(A.is_running() && RUNS.load(Ordering::SeqCst) == 4);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_respawn_waits_out_the_previous_instances_storage_release() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (runs={}, running={})",
                RUNS.load(Ordering::SeqCst),
                A.is_running(),
            )
        },
    );
}
