use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;

struct Probe {
    runs: u32,
}

static OBSERVED_RUNS: AtomicU32 = AtomicU32::new(0);
static ROUNDS: AtomicU32 = AtomicU32::new(0);
static WORK: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static DONE: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU32 = AtomicU32::new(0);

async fn looper(probe: &mut Probe) -> ! {
    probe.runs += 1;
    OBSERVED_RUNS.store(probe.runs, Ordering::SeqCst);
    loop {
        ROUNDS.fetch_add(1, Ordering::SeqCst);
        WORK.wait().await;
    }
}

async fn oneshot() -> u32 {
    7
}

async fn with_extra(n: u32) -> u32 {
    n
}

supervisor_graph! {
    node LOOPER = Terminate, deps: [], task: looper, cancel,
        resources: [PROBE: Probe];
    node ONESHOT = Terminate, deps: [], task: oneshot, cancel, exit: u32;
    node EXTRA = Terminate, deps: [], task: with_extra(9), cancel, exit: u32;
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
    settle(|| ROUNDS.load(Ordering::SeqCst) >= 1).await;
    assert_eq!(OBSERVED_RUNS.load(Ordering::SeqCst), 1, "first activation");

    assert_eq!(
        ONESHOT_EXIT.wait_take().await,
        7,
        "a cancel worker that returns still provides its exit value"
    );
    assert!(ONESHOT.has_exited(), "and still records the completion");
    assert_eq!(
        EXTRA_EXIT.wait_take().await,
        9,
        "the partial-call form reaches the worker with an empty injected lead"
    );
    PHASE.store(1, Ordering::SeqCst);

    sup.stop_node(&LOOPER)
        .await
        .expect("acked by the generated shell, not by the worker");
    assert!(!LOOPER.is_running(), "stopped");
    assert!(LOOPER.has_exited(), "and recorded as exited");
    let rounds_at_stop = ROUNDS.load(Ordering::SeqCst);
    PHASE.store(2, Ordering::SeqCst);

    sup.start_node(&LOOPER, &spawner)
        .await
        .expect("respawn after cancel");
    settle(|| OBSERVED_RUNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        OBSERVED_RUNS.load(Ordering::SeqCst),
        2,
        "same Probe instance restored and re-taken (a fresh one would read 1)"
    );
    PHASE.store(3, Ordering::SeqCst);

    let rounds_before = ROUNDS.load(Ordering::SeqCst);
    sup.stop_node(&LOOPER).await.expect("second stop");
    WORK.signal(1);
    settle(|| false).await;
    assert_eq!(
        ROUNDS.load(Ordering::SeqCst),
        rounds_before,
        "the aborted worker is dropped, not merely ignored"
    );
    assert!(rounds_at_stop >= 1);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn cancel_flag_races_acks_and_restores() {
    let _clock = MockDriver::get();
    PROBE.provide(Probe { runs: 0 });

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
            "did not complete (phase={}, rounds={}, runs={})",
            PHASE.load(Ordering::SeqCst),
            ROUNDS.load(Ordering::SeqCst),
            OBSERVED_RUNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
