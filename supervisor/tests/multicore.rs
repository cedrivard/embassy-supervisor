use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph, trace};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    executor CORE1;
    node REMOTE = Terminate, deps: [], executor: CORE1, spawn: remote_task;
    node CA = Pause, deps: [];
    node CB = Pause, deps: [];
}

static FAKE_CORE: AtomicUsize = AtomicUsize::new(0);
fn fake_core() -> usize {
    FAKE_CORE.load(Ordering::Relaxed)
}

static REMOTE_STARTED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

#[embassy_executor::task]
async fn remote_task(node: &'static TaskNode) {
    REMOTE_STARTED.store(true, Ordering::Release);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// Runs on executor A ("core 0"): rendezvous with B, start the graph (which
/// spawns REMOTE onto B through the CORE1 slot), then stop it cross-thread.
#[embassy_executor::task]
async fn driver_task(spawner: Spawner) {
    // (2) + (3) Start the graph WITHOUT pre-filling the slot: thread B publishes
    // CORE1's SendSpawner ~50 ms late (below), so `Supervisor::start` takes the
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner)
        .await
        .expect("start spawns REMOTE via the slot once B fills it");
    while !REMOTE_STARTED.load(Ordering::Acquire) {
        embassy_futures::yield_now().await;
    }
    assert!(REMOTE.is_running());

    sup.stop_node(&REMOTE).await.expect("stop_node");
    assert!(!REMOTE.is_running());

    DONE.store(true, Ordering::Release);
}

#[test]
fn multicore() {
    let clock = MockDriver::get();
    trace::register_graph(GRAPH.graph_ref);

    trace::set_core_id_fn(fake_core);
    const EXEC_C0: u32 = 0xa0;
    const EXEC_C1: u32 = 0xa1;
    CA.set_task_id(71);
    CB.set_task_id(72);

    FAKE_CORE.store(0, Ordering::Relaxed);
    trace::on_task_exec_begin(EXEC_C0, 71);
    clock.advance(Duration::from_ticks(10));
    FAKE_CORE.store(1, Ordering::Relaxed);
    trace::on_task_exec_begin(EXEC_C1, 72);
    clock.advance(Duration::from_ticks(20));
    FAKE_CORE.store(0, Ordering::Relaxed);
    trace::on_task_exec_end(EXEC_C0, 71);
    FAKE_CORE.store(1, Ordering::Relaxed);
    clock.advance(Duration::from_ticks(5));
    trace::on_task_exec_end(EXEC_C1, 72);

    assert_eq!(CA.exec_ticks(), 30, "core 0 poll exact (10 + 20 overlap)");
    assert_eq!(CB.exec_ticks(), 25, "core 1 poll exact (20 overlap + 5)");
    assert_eq!(CA.max_poll_ticks(), 30, "no cross-charge into core 0");
    assert_eq!(CB.max_poll_ticks(), 25, "no cross-charge into core 1");
    FAKE_CORE.store(0, Ordering::Relaxed);
    trace::on_task_exec_begin(EXEC_C0, 71);
    clock.advance(Duration::from_ticks(3));
    trace::on_task_exec_end(EXEC_C0, 71);
    assert_eq!(CA.exec_ticks(), 33, "no stolen residue on core 0");

    FAKE_CORE.store(0, Ordering::Relaxed);

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver_task(spawner).unwrap());
        });
    });
    // Thread B: "core 1" — its executor's only supervisor-visible artifact is
    std::thread::spawn(|| {
        std::thread::sleep(StdDuration::from_millis(50));
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            CORE1.set(spawner.make_send());
        });
    });

    // The executor threads never exit; poll the completion flag with a deadline.
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::Acquire) {
        assert!(
            StdInstant::now() < deadline,
            "cross-thread start/stop did not complete (REMOTE_STARTED = {})",
            REMOTE_STARTED.load(Ordering::Acquire)
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
    assert!(REMOTE_STARTED.load(Ordering::Acquire));
    assert_eq!(
        REMOTE.task_id(),
        0,
        "task_end cleared the id after the stop"
    );
}
