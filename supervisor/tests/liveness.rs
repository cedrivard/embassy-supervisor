use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node WORKER = Terminate, deps: [];
}

static DONE: AtomicBool = AtomicBool::new(false);

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    let sup = Supervisor::new(&GRAPH);

    assert!(!WORKER.is_running());
    clock.advance(Duration::from_secs(5));
    assert!(!WORKER.is_stale(Duration::from_millis(100)));

    sup.start(&spawner).await.expect("start");
    assert!(WORKER.is_running());

    assert_eq!(WORKER.ticks_since_beat(), 0);
    assert!(!WORKER.is_stale(Duration::from_secs(1)));

    clock.advance(Duration::from_secs(2));
    assert!(WORKER.is_stale(Duration::from_secs(1)));

    WORKER.beat();
    assert!(!WORKER.is_stale(Duration::from_secs(1)));
    clock.advance(Duration::from_millis(999));
    assert!(!WORKER.is_stale(Duration::from_secs(1)));
    clock.advance(Duration::from_millis(2));
    assert!(WORKER.is_stale(Duration::from_secs(1)));

    // A stall past the u32 tick wrap stays a stall: the age saturates.
    let age = u64::from(WORKER.ticks_since_beat());
    clock.advance(Duration::from_ticks(1 << 31));
    assert_eq!(WORKER.ticks_since_beat(), u32::MAX / 2);
    clock.advance(Duration::from_ticks((1 << 31) - age));
    assert!(
        WORKER.is_stale(Duration::from_secs(1)),
        "2^32 ticks after the last beat, still stale"
    );
    assert!(
        WORKER.is_stale(Duration::from_ticks(1 << 40)),
        "a max_age past the cap is clamped"
    );

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
