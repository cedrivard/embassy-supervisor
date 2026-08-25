use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

static SPAWNS: AtomicU32 = AtomicU32::new(0);
static RESUMES: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn sensor_worker(node: &'static TaskNode) {
    SPAWNS.fetch_add(1, Ordering::SeqCst);
    loop {
        if node
            .run_cancellable(core::future::pending::<()>())
            .await
            .is_err()
        {
            node.ack_dropped();
            node.wait_resume().await;
            RESUMES.fetch_add(1, Ordering::SeqCst);
        }
    }
}

supervisor_graph! {
    node SENSOR = Pause, deps: [], task: sensor_worker;
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

    // Stopped at boot: disabled before start(), so start() skips it and no
    // instance exists.
    SENSOR.set_disabled(true);
    sup.start(&spawner).await.expect("bring-up");
    assert!(!SENSOR.is_running() && SPAWNS.load(Ordering::SeqCst) == 0);

    // First Activate: down and not parked, so the wave takes the spawn path.
    sup.activate(&SENSOR, &spawner).await;
    settle(|| SPAWNS.load(Ordering::SeqCst) == 1).await;
    assert!(SENSOR.is_running(), "a real instance, not a ghost");

    // A stop parks the instance (ack without exit)...
    sup.stop_node(&SENSOR).await.expect("the instance acks");
    assert!(!SENSOR.is_running());

    // ...and the next Activate resumes it in place: no second spawn.
    sup.activate(&SENSOR, &spawner).await;
    settle(|| RESUMES.load(Ordering::SeqCst) == 1).await;
    assert_eq!(
        RESUMES.load(Ordering::SeqCst),
        1,
        "the parked worker passed wait_resume() — running alone would not \
         prove the instance woke"
    );
    assert!(SENSOR.is_running());
    assert_eq!(
        SPAWNS.load(Ordering::SeqCst),
        1,
        "the parked instance was resumed, not respawned"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn activate_spawns_when_down_and_resumes_when_parked() {
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
            "did not complete (spawns={}, resumes={}, running={})",
            SPAWNS.load(Ordering::SeqCst),
            RESUMES.load(Ordering::SeqCst),
            SENSOR.is_running(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
