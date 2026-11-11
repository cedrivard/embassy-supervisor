mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Aborted, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

use support::settle;

supervisor_graph! {
    node SCORE = Terminate, deps: [], task: score_worker, exit: u32;
    node SERVE = Terminate, deps: [], task: serve_worker, exit: Result<u32, Aborted>, disabled;
}

static SCORE_RUNS: AtomicU32 = AtomicU32::new(0);
static SERVE_WORK: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn score_worker(_node: &'static TaskNode) -> u32 {
    40 + SCORE_RUNS.fetch_add(1, Ordering::SeqCst)
}

/// The documented idiom: the body IS `run_cancellable`, its `Result<R, Aborted>`
/// is the exit value, and the abort arm acks before returning.
async fn serve_worker(node: &'static TaskNode) -> Result<u32, Aborted> {
    node.run_cancellable_acked(SERVE_WORK.wait()).await
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start");

    settle("score exited", || SCORE.has_exited()).await;
    assert_eq!(
        SCORE_EXIT.take(),
        Some(40),
        "the shell fills the exit slot before has_exited reads true"
    );
    assert!(SCORE_EXIT.take().is_none(), "take consumed the value");
    PHASE.store(1, Ordering::SeqCst);

    sup.respawn_terminate(&spawner).await.expect("respawn 1");
    settle("score ran twice", || SCORE_RUNS.load(Ordering::SeqCst) == 2).await;
    settle("score exited again", || SCORE.has_exited()).await;
    sup.respawn_terminate(&spawner).await.expect("respawn 2");
    settle("score ran three times", || {
        SCORE_RUNS.load(Ordering::SeqCst) == 3
    })
    .await;
    settle("score exited a third time", || SCORE.has_exited()).await;
    assert_eq!(
        SCORE_EXIT.wait_take().await,
        42,
        "unread 41 was overwritten by the newer completion — mailbox, not log"
    );
    PHASE.store(2, Ordering::SeqCst);

    sup.start_node(&SERVE, &spawner).await.expect("start serve");
    SERVE_WORK.signal(9);
    settle("serve exited", || SERVE.has_exited()).await;
    assert_eq!(
        SERVE_EXIT.wait_take().await,
        Ok(9),
        "completion recorded as Ok(output)"
    );
    sup.start_node(&SERVE, &spawner)
        .await
        .expect("restart serve");
    settle("serve running", || SERVE.is_running()).await;
    sup.stop_node(&SERVE).await.expect("combinator acks");
    assert_eq!(
        SERVE_EXIT.wait_take().await,
        Err(Aborted),
        "cancellation recorded as Err(Aborted)"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn exit_values_round_trip() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (phase={}, score_runs={})",
                PHASE.load(Ordering::SeqCst),
                SCORE_RUNS.load(Ordering::SeqCst),
            )
        },
    );
}
