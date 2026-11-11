use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    executor TIER;
    node REMOTE = Terminate, deps: [], executor: TIER, spawn: remote_task, slot_timeout: 500;
}

static STARTS: AtomicU32 = AtomicU32::new(0);
static RESPAWN_CALLED: AtomicBool = AtomicBool::new(false);
static RELEASE: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static FAULT_PHASE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

/// A hand-written `spawn:` task on another executor that acks, then holds its
/// task slot until released: the supervisor sees the ack while the slot is
/// still taken, so the respawn is `Busy` until the task returns.
#[embassy_executor::task]
async fn remote_task(node: &'static TaskNode) {
    STARTS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
    RELEASE.wait().await;
}

/// Executor passes: the probe is polled once per pass, so a move of N means
/// every task woken before it has been polled N times over.
static PASSES: AtomicU32 = AtomicU32::new(0);
static PROBE_STOP: AtomicBool = AtomicBool::new(false);

/// Ends after the test's last barrier, so the executor idles instead of
/// spinning for the life of the binary.
#[embassy_executor::task]
async fn pass_probe() {
    while !PROBE_STOP.load(Ordering::SeqCst) {
        PASSES.fetch_add(1, Ordering::SeqCst);
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start");
    while STARTS.load(Ordering::SeqCst) < 1 {
        embassy_futures::yield_now().await;
    }

    sup.stop_node(&REMOTE).await.expect("acked");
    RESPAWN_CALLED.store(true, Ordering::SeqCst);
    sup.start_node(&REMOTE, &spawner)
        .await
        .expect("the respawn goes through once the old task returns");
    while STARTS.load(Ordering::SeqCst) < 2 {
        embassy_futures::yield_now().await;
    }

    // Never released: the slot stays taken past slot_timeout.
    sup.stop_node(&REMOTE).await.expect("acked");
    FAULT_PHASE.store(true, Ordering::SeqCst);
    let err = sup
        .start_node(&REMOTE, &spawner)
        .await
        .expect_err("a task that never returns keeps its slot");
    assert!(matches!(err.kind, FaultKind::Spawn(_)), "{err}");
    assert!(!REMOTE.is_running());

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn respawn_after_ack_waits_for_the_task_to_return() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
            spawner.spawn(pass_probe().unwrap());
        });
    });
    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| TIER.set(spawner.make_send()));
    });

    // First stop: release only once 50 passes of the driver's executor have
    // gone by with the respawn pending. The clock stands still, so the respawn
    // cannot time out: its first attempt is a guaranteed `Busy`, answered with
    // one fence pass, and every later one sleeps on the 1 ms backoff.
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !RESPAWN_CALLED.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "the respawn was never called");
        std::thread::sleep(StdDuration::from_millis(1));
    }
    let seen = PASSES.load(Ordering::SeqCst);
    while PASSES.load(Ordering::SeqCst) < seen + 50 {
        assert!(
            StdInstant::now() < deadline,
            "the driver's executor stalled"
        );
        std::thread::sleep(StdDuration::from_millis(1));
    }
    assert_eq!(
        STARTS.load(Ordering::SeqCst),
        1,
        "still held by the old task"
    );
    PROBE_STOP.store(true, Ordering::SeqCst);
    RELEASE.signal(());

    // The old task returns on its own executor, which no event announces: the
    // respawn goes through on its next backoff, so the clock moves 1 ms a step,
    // far inside the 500 ms gate budget.
    while STARTS.load(Ordering::SeqCst) < 2 {
        assert!(
            StdInstant::now() < deadline,
            "the respawn never went through"
        );
        clock.advance(Duration::from_millis(1));
        std::thread::sleep(StdDuration::from_millis(2));
    }

    // Second stop: the clock moves 50 ms a step, to fault the held respawn.
    while !DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "did not complete");
        if FAULT_PHASE.load(Ordering::SeqCst) {
            clock.advance(Duration::from_millis(50));
        }
        std::thread::sleep(StdDuration::from_millis(2));
    }
    RELEASE.signal(());
}
