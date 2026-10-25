use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    FaultKind, LifecycleKind, Supervisor, TaskNode, lifecycle_events_dropped, supervisor_graph,
    try_wait_lifecycle,
};
use embassy_time::MockDriver;

supervisor_graph! {
    node PARK = Pause, deps: [], task: park;
    node LEAF = Terminate, deps: [PARK], task: leaf;
    node DEAF = Terminate, deps: [], task: deaf, ack_timeout: 200;
}

static DONE: AtomicBool = AtomicBool::new(false);

/// Acks a stop and parks: the instance survives, so its ack is `Stopped`.
async fn park(node: &'static TaskNode) {
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
    }
}

/// Acks a stop by returning: the instance is gone, so its ack is `Exited`.
async fn leaf(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

/// Never acks.
async fn deaf(_node: &'static TaskNode) {
    core::future::pending::<()>().await;
}

async fn settle() {
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

fn drain() -> Vec<(&'static str, LifecycleKind)> {
    let mut out = Vec::new();
    while let Some(ev) = try_wait_lifecycle() {
        out.push((ev.node.name(), ev.kind));
    }
    out
}

fn names(events: &[(&'static str, LifecycleKind)]) -> Vec<(&'static str, &'static str)> {
    events.iter().map(|(n, k)| (*n, k.as_str())).collect()
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    sup.start(&spawner).await.expect("start");
    settle().await;
    let events = drain();
    let started = |name| {
        events
            .iter()
            .position(|(n, k)| *n == name && matches!(k, LifecycleKind::Started))
            .unwrap_or_else(|| panic!("no Started for {name}: {:?}", names(&events)))
    };
    assert!(
        started("park") < started("leaf"),
        "a dependency is reported started before its dependent: {:?}",
        names(&events)
    );
    started("deaf");
    assert_eq!(events.len(), 3, "one event per spawn: {:?}", names(&events));

    sup.stop_node(&LEAF).await.expect("stop leaf");
    assert_eq!(
        names(&drain()),
        [("leaf", "stop_requested"), ("leaf", "exited")],
        "a body that returns on the stop is Exited"
    );

    sup.stop_node(&PARK).await.expect("stop park");
    assert_eq!(
        names(&drain()),
        [("park", "stop_requested"), ("park", "stopped")],
        "a parked Pause instance is Stopped, not Exited"
    );

    sup.resume_node(&PARK);
    assert_eq!(names(&drain()), [("park", "resumed")]);

    let err = sup
        .stop_node(&DEAF)
        .await
        .expect_err("a worker that never acks faults the stop");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout));
    let events = drain();
    assert_eq!(
        names(&events),
        [("deaf", "stop_requested"), ("deaf", "faulted")]
    );
    assert!(
        matches!(
            events[1].1,
            LifecycleKind::Faulted(FaultKind::ShutdownTimeout)
        ),
        "the event carries the fault the verb returned"
    );

    // Overflow: three start/stop cycles post nine events into eight slots.
    assert_eq!(lifecycle_events_dropped(), 0, "nothing dropped so far");
    for _ in 0..3 {
        sup.start_node(&LEAF, &spawner).await.expect("start leaf");
        settle().await;
        sup.stop_node(&LEAF).await.expect("stop leaf");
    }
    assert_eq!(
        lifecycle_events_dropped(),
        1,
        "the ninth event fell off a full queue"
    );
    let events = drain();
    assert_eq!(events.len(), 8, "the queue held its capacity");
    assert_eq!(
        names(&events[..3]),
        [
            ("leaf", "started"),
            ("leaf", "stop_requested"),
            ("leaf", "exited")
        ],
        "the head is kept, the newest event is the one dropped"
    );

    sup.start_node(&LEAF, &spawner).await.expect("start leaf");
    assert_eq!(
        names(&drain()),
        [("leaf", "started")],
        "events flow again once drained"
    );
    assert_eq!(lifecycle_events_dropped(), 1);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn reports_every_driven_transition_and_counts_drops() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    // The missed-ack phase needs the clock to pass DEAF's ack window.
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "did not complete");
        clock.advance(embassy_time::Duration::from_millis(50));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
