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
