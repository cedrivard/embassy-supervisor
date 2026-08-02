//! Behavioral tests for the `liveness` feature: `beat()` stamps freshness,
//! `is_stale(max_age)` flips once the (mock) clock outruns the last beat, a
//! fresh spawn is never instantly stale (`set_running` stamps a beat), and a
//! not-running node is never stale (down is `is_running`'s business). The
//! liveness API is synchronous, so the driver task advances the mock clock
//! inline — no cross-thread phase dance needed. Harness as teardown.rs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    // Parked (no spawn:): start() marks it running without spawning anything —
    // the liveness API is atomics + clock, so no task body is needed.
    node WORKER = Terminate, deps: [];
}

static DONE: AtomicBool = AtomicBool::new(false);

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    let sup = Supervisor::new(&GRAPH);

    // Not running: never stale, whatever the clock says.
    assert!(!WORKER.is_running());
    clock.advance(Duration::from_secs(5));
    assert!(!WORKER.is_stale(Duration::from_millis(100)));

    sup.start(spawner).await.expect("start");
    assert!(WORKER.is_running());

    // Fresh spawn: set_running stamped a beat, so not instantly stale.
    assert_eq!(WORKER.ticks_since_beat(), 0);
    assert!(!WORKER.is_stale(Duration::from_secs(1)));

    // Clock outruns the spawn stamp: stale.
    clock.advance(Duration::from_secs(2));
    assert!(WORKER.is_stale(Duration::from_secs(1)));

    // A beat refreshes it; staleness needs strictly more than max_age.
    WORKER.beat();
    assert!(!WORKER.is_stale(Duration::from_secs(1)));
    clock.advance(Duration::from_millis(999));
    assert!(!WORKER.is_stale(Duration::from_secs(1)));
    clock.advance(Duration::from_millis(2));
    assert!(WORKER.is_stale(Duration::from_secs(1)));

    // Exiting clears running, and with it staleness. (A parked node's
    // app-spawned task would call this on its way out; teardown would await
    // an ack no task exists to give, so the exit record is the right tool.)
    WORKER.mark_exited();
    assert!(!WORKER.is_running());
    assert!(WORKER.has_exited());
    assert!(!WORKER.is_stale(Duration::from_secs(1)));
    let _ = &sup;

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn beats_and_staleness() {
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
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
