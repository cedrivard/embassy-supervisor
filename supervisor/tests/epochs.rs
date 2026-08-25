use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;

supervisor_graph! {
    node PROVIDER = Terminate, deps: [], task: provider;
    node WATCHER = Terminate, deps: [PROVIDER], task: watcher;
    node PARKED = Pause, deps: [], task: parked;
}

static SAMPLE_NOW: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static WAIT_CHANGE: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static SAMPLED: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static DONE: AtomicBool = AtomicBool::new(false);

async fn provider(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

/// Stays up across the provider's restart and reports what it can see — the
async fn watcher(node: &'static TaskNode) {
    loop {
        match select3(node.wait_shutdown(), SAMPLE_NOW.wait(), WAIT_CHANGE.wait()).await {
            Either3::First(()) => {
                node.mark_exited();
                return;
            }
            Either3::Second(()) => SAMPLED.signal(PROVIDER.epoch()),
            // The seen value rides in the signal, so arming late (after the
            // restart already happened) still returns instead of parking.
            Either3::Third(seen) => {
                SAMPLED.signal(PROVIDER.wait_epoch_change(seen).await);
            }
        }
    }
}

async fn parked(node: &'static TaskNode) {
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    assert_eq!(
        PROVIDER.epoch(),
        0,
        "un-spawned node starts at generation 0"
    );
    assert_eq!(WATCHER.epoch(), 0);

    sup.start(&spawner).await.expect("start");
    assert_eq!(PROVIDER.epoch(), 1, "first spawn is generation 1");
    assert_eq!(WATCHER.epoch(), 1);
    assert_eq!(PARKED.epoch(), 1);

    SAMPLE_NOW.signal(());
    assert_eq!(SAMPLED.wait().await, 1, "watcher's initial sample");

    WAIT_CHANGE.signal(PROVIDER.epoch());
    sup.stop_node(&PROVIDER).await.expect("stop provider");
    assert!(!PROVIDER.is_running());
    assert!(WATCHER.is_running(), "the dependent is left running");
    sup.start_node(&PROVIDER, &spawner)
        .await
        .expect("restart provider");

    assert_eq!(PROVIDER.epoch(), 2, "stop + start bumps the generation");
    assert_eq!(WATCHER.epoch(), 1, "an untouched node's epoch is unchanged");

    assert_eq!(
        SAMPLED.wait().await,
        2,
        "wait_epoch_change parked across the restart and returned the new \
         generation"
    );

    SAMPLE_NOW.signal(());
    assert_eq!(
        SAMPLED.wait().await,
        2,
        "the still-running dependent observes its provider's restart"
    );

    sup.deactivate(&PARKED).await.expect("pause parked");
    assert!(!PARKED.is_running());
    assert_eq!(PARKED.epoch(), 1, "pausing does not bump");
    sup.activate(&PARKED, &spawner).await;
    assert_eq!(
        PARKED.epoch(),
        2,
        "a Pause resume bumps (activation generation)"
    );

    sup.teardown().await.expect("teardown");
    sup.respawn_terminate(&spawner).await.expect("respawn");
    assert_eq!(
        PROVIDER.epoch(),
        3,
        "respawn_terminate goes through the same choke point"
    );
    assert_eq!(WATCHER.epoch(), 2);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn epoch_tracks_every_activation() {
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
