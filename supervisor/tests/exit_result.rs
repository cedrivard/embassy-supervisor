use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Aborted, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;

supervisor_graph! {
    node SCORE = Terminate, deps: [], task: score_worker, pool_size: 2, exit: u32;
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

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start");

    settle(|| SCORE.has_exited()).await;
    assert_eq!(
        SCORE_EXIT.wait_take().await,
        40,
        "first run's return value read from the exit slot"
    );
    assert!(SCORE_EXIT.take().is_none(), "wait_take consumed the value");
    PHASE.store(1, Ordering::SeqCst);

    sup.respawn_terminate(&spawner).await.expect("respawn 1");
    settle(|| SCORE_RUNS.load(Ordering::SeqCst) == 2).await;
    settle(|| SCORE.has_exited()).await;
    sup.respawn_terminate(&spawner).await.expect("respawn 2");
    settle(|| SCORE_RUNS.load(Ordering::SeqCst) == 3).await;
    settle(|| SCORE.has_exited()).await;
    assert_eq!(
        SCORE_EXIT.wait_take().await,
        42,
        "unread 41 was overwritten by the newer completion — mailbox, not log"
    );
    PHASE.store(2, Ordering::SeqCst);

    sup.start_node(&SERVE, &spawner).await.expect("start serve");
    SERVE_WORK.signal(9);
    settle(|| SERVE.has_exited()).await;
    assert_eq!(
        SERVE_EXIT.wait_take().await,
        Ok(9),
        "completion recorded as Ok(output)"
    );
    sup.start_node(&SERVE, &spawner)
        .await
        .expect("restart serve");
    settle(|| SERVE.is_running()).await;
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
    let _clock = MockDriver::get();

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
            "did not complete (phase={}, score_runs={})",
            PHASE.load(Ordering::SeqCst),
            SCORE_RUNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
