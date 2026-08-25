use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

supervisor_graph! {
    node PARK = Pause, deps: [], task: park, slot_timeout: 5000;

    node DEP = Terminate, deps: [PARK ready bound], task: dep, slot_timeout: 5000;
}

static PARK_STARTS: AtomicU32 = AtomicU32::new(0);
static DEP_STARTS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn park(node: &'static TaskNode) {
    loop {
        PARK_STARTS.fetch_add(1, Ordering::SeqCst);
        node.set_ready();
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
    }
}

async fn dep(node: &'static TaskNode) {
    DEP_STARTS.fetch_add(1, Ordering::SeqCst);
    node.set_ready();
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn settle() {
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    sup.start(&spawner).await.expect("start");
    settle().await;
    assert_eq!(PARK_STARTS.load(Ordering::SeqCst), 1);
    assert_eq!(DEP_STARTS.load(Ordering::SeqCst), 1);
    assert!(PARK.is_running());
    assert!(DEP.is_running());

    sup.restart(&PARK, &spawner).await.expect("restart");
    settle().await;

    assert_eq!(
        PARK_STARTS.load(Ordering::SeqCst),
        2,
        "the Pause node was resumed, not respawned"
    );
    assert!(PARK.is_running(), "and is running again");

    assert_eq!(
        DEP_STARTS.load(Ordering::SeqCst),
        2,
        "the dependent was cycled with the Pause target"
    );
    assert!(DEP.is_running());

    sup.stop_node(&PARK).await.expect("stop park");
    settle().await;
    assert!(!PARK.is_running(), "PARK is paused");
    sup.apply_bind(&spawner).await.expect("bind cascade");
    settle().await;

    assert!(
        !DEP.is_running(),
        "the bound dependent was stopped by the cascade"
    );
    assert!(
        DEP.is_bound_stopped(),
        "DEP is bound-stopped (a bound provider withdrew)"
    );

    sup.activate(&PARK, &spawner).await;
    settle().await;
    sup.apply_bind(&spawner).await.expect("bind cascade");
    settle().await;

    assert!(PARK.is_running(), "PARK is resumed");
    assert_eq!(
        PARK_STARTS.load(Ordering::SeqCst),
        3,
        "PARK re-entered its loop on resume"
    );
    assert!(
        DEP.is_running(),
        "the bound dependent was restarted after the Pause provider resumed"
    );
    assert!(!DEP.is_bound_stopped(), "the flag lifted on recovery");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn pause_node_under_restart_and_bind() {
    let clock = MockDriver::get();

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
        clock.advance(embassy_time::Duration::from_millis(100));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
