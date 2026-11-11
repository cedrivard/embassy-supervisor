mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;
use support::settle;

supervisor_graph! {
    node HELD = Terminate, deps: [], task: held_worker;
}

static ACTIVATIONS: AtomicU32 = AtomicU32::new(0);
static STOPS: AtomicU32 = AtomicU32::new(0);
static RELEASE: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static DONE: AtomicBool = AtomicBool::new(false);

/// Acks its stop itself, then is held before returning. On hardware the same
/// window opens whenever the supervisor (a higher-priority tier) preempts a
/// worker between its own ack and the shell's epilogue: the supervisor has the
/// ack, the activation is not over yet.
async fn held_worker(node: &'static TaskNode) {
    ACTIVATIONS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    STOPS.fetch_add(1, Ordering::SeqCst);
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
    settle("first activation", || {
        ACTIVATIONS.load(Ordering::SeqCst) == 1
    })
    .await;

    for round in 1..=2 {
        sup.restart(&HELD, &spawner)
            .await
            .expect("the restart waits for the held activation to finish");
        settle("the restart's activation", || {
            ACTIVATIONS.load(Ordering::SeqCst) == round + 1
        })
        .await;
        assert!(HELD.is_running());
        assert!(
            !HELD.has_exited(),
            "round {round}: the new activation carries none of the old one's exit"
        );
    }
    assert_eq!(
        STOPS.load(Ordering::SeqCst),
        2,
        "each restart signalled the activation it found running"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn restart_waits_for_a_held_epilogue_and_never_inherits_its_flags() {
    let _clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
            spawner.spawn(pass_probe().unwrap());
        });
    });

    // Release each held activation only once the supervisor has collected its
    // ack (HELD reads stopped) and its executor has run 50 more passes, so the
    // restart is parked waiting for the shell. The clock never moves: nothing
    // here may wait on a timeout.
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    for round in 1..=2 {
        while STOPS.load(Ordering::SeqCst) < round || HELD.is_running() {
            assert!(
                StdInstant::now() < deadline,
                "round {round}: the ack was never collected"
            );
            std::thread::sleep(StdDuration::from_millis(1));
        }
        let seen = PASSES.load(Ordering::SeqCst);
        while PASSES.load(Ordering::SeqCst) < seen + 50 {
            assert!(
                StdInstant::now() < deadline,
                "round {round}: the executor stalled"
            );
            std::thread::sleep(StdDuration::from_millis(1));
        }
        if round == 2 {
            PROBE_STOP.store(true, Ordering::SeqCst);
        }
        RELEASE.signal(());
    }
    while !DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
