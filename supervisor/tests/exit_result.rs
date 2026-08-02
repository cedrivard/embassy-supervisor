//! Behavioral tests for the `exit: Type` clause: the generated shell
//! `provide()`s the worker's return value into `<NODE>_EXIT` before
//! `mark_exited()`, `wait_take()` observes it, a respawn's completion
//! overwrites an unread value (mailbox, not log), and the documented idiom —
//! a worker returning `Result<R, Aborted>` straight out of `run_cancellable`
//! with `exit: Result<R, Aborted>` — records completed-vs-cancelled.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Aborted, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;

supervisor_graph! {
    node SCORE = Terminate, deps: [], task: score_worker, pool_size: 2, exit: u32;
    // `disabled`: driven by start_node below, and kept out of the two
    // respawn_terminate sweeps the SCORE overwrite scenario runs (a live parked
    // SERVE instance would exhaust its pool_size-1 shell on the second sweep).
    node SERVE = Terminate, deps: [], task: serve_worker, exit: Result<u32, Aborted>, disabled;
}

static SCORE_RUNS: AtomicU32 = AtomicU32::new(0);
/// Feeds SERVE's cancellable work future.
static SERVE_WORK: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// Returns a per-run score; the shell provides it into `SCORE_EXIT`.
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
    sup.start(spawner).await.expect("start");

    // ── exit value present once the completion is recorded ──────────────────
    settle(|| SCORE.has_exited()).await;
    assert_eq!(
        SCORE_EXIT.wait_take().await,
        40,
        "first run's return value read from the exit slot"
    );
    assert!(SCORE_EXIT.take().is_none(), "wait_take consumed the value");
    PHASE.store(1, Ordering::SeqCst);

    // ── respawn overwrites an unread value: run twice, read once ────────────
    sup.respawn_terminate(spawner).await.expect("respawn 1");
    settle(|| SCORE_RUNS.load(Ordering::SeqCst) == 2).await;
    settle(|| SCORE.has_exited()).await;
    sup.respawn_terminate(spawner).await.expect("respawn 2");
    settle(|| SCORE_RUNS.load(Ordering::SeqCst) == 3).await;
    settle(|| SCORE.has_exited()).await;
    assert_eq!(
        SCORE_EXIT.wait_take().await,
        42,
        "unread 41 was overwritten by the newer completion — mailbox, not log"
    );
    PHASE.store(2, Ordering::SeqCst);

    // ── completed-vs-cancelled through exit: Result<R, Aborted> ─────────────
    sup.start_node(&SERVE, spawner).await.expect("start serve");
    SERVE_WORK.signal(9);
    settle(|| SERVE.has_exited()).await;
    assert_eq!(
        SERVE_EXIT.wait_take().await,
        Ok(9),
        "completion recorded as Ok(output)"
    );
    // Start again, then stop mid-wait: the exit value is Err(Aborted).
    sup.start_node(&SERVE, spawner)
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
