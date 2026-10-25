use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    LifecycleEvent, Supervisor, TaskNode, set_lifecycle_sink, supervisor_graph,
};

supervisor_graph! {
    node PARK = Pause, deps: [], task: park;
    node LEAF = Terminate, deps: [PARK], task: leaf;
}

static DONE: AtomicBool = AtomicBool::new(false);
static SEEN: Mutex<Vec<(&'static str, &'static str)>> = Mutex::new(Vec::new());

fn record(ev: LifecycleEvent) {
    SEEN.lock()
        .unwrap()
        .push((ev.node.name(), ev.kind.as_str()));
}

fn take() -> Vec<(&'static str, &'static str)> {
    core::mem::take(&mut *SEEN.lock().unwrap())
}

async fn park(node: &'static TaskNode) {
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
    }
}

async fn leaf(node: &'static TaskNode) {
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
    set_lifecycle_sink(record);

    sup.start(&spawner).await.expect("start");
    settle().await;
    assert_eq!(
        take(),
        [("park", "started"), ("leaf", "started")],
        "bring-up reaches the sink in dependency order"
    );

    sup.stop_node(&LEAF).await.expect("stop leaf");
    sup.stop_node(&PARK).await.expect("stop park");
    assert_eq!(
        take(),
        [
            ("leaf", "stop_requested"),
            ("leaf", "exited"),
            ("park", "stop_requested"),
            ("park", "stopped"),
        ],
        "each ack reaches the sink after its request, Exited vs Stopped kept"
    );

    // Far more than the queue's 8 slots, with nobody draining in between: the
    // sink is called synchronously, so nothing can fall off.
    sup.resume_node(&PARK);
    for _ in 0..5 {
        sup.start_node(&LEAF, &spawner).await.expect("start leaf");
        settle().await;
        sup.stop_node(&LEAF).await.expect("stop leaf");
    }
    let seen = take();
    assert_eq!(seen.len(), 1 + 5 * 3, "every event delivered: {seen:?}");
    assert_eq!(seen[0], ("park", "resumed"));
    for cycle in seen[1..].chunks(3) {
        assert_eq!(
            cycle,
            [
                ("leaf", "started"),
                ("leaf", "stop_requested"),
                ("leaf", "exited")
            ]
        );
    }

    // With the queue compiled in as well, each event reaches both observers.
    #[cfg(feature = "lifecycle-events")]
    {
        while embassy_supervisor::try_wait_lifecycle().is_some() {}
        sup.start_node(&LEAF, &spawner).await.expect("start leaf");
        let queued = embassy_supervisor::try_wait_lifecycle().expect("queued too");
        assert_eq!(
            (queued.node.name(), queued.kind.as_str()),
            ("leaf", "started")
        );
        assert_eq!(take(), [("leaf", "started")]);
    }

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn calls_the_sink_for_every_transition_in_order() {
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
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
