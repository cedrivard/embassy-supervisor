mod support;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

std::thread_local! {
    /// The "core" the calling thread runs on: each executor thread sets its
    /// own, so a hook polled on executor B never reads A's id.
    static CORE_ID: Cell<usize> = const { Cell::new(0) };
}
fn set_core(id: usize) {
    CORE_ID.with(|c| c.set(id));
}
fn fake_core() -> usize {
    CORE_ID.with(Cell::get)
}

static REMOTE_STARTED: AtomicBool = AtomicBool::new(false);
static START_ENTERED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

#[embassy_executor::task]
async fn remote_task(node: &'static TaskNode) {
    REMOTE_STARTED.store(true, Ordering::Release);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
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

/// Runs on executor A ("core 0"): rendezvous with B, start the graph (which
/// spawns REMOTE onto B through the CORE1 slot), then stop it cross-thread.
#[embassy_executor::task]
async fn driver_task(spawner: Spawner) {
    // Start the graph without pre-filling the slot: thread B publishes
    // CORE1's SendSpawner only once this call is parked on it, so `start`
    // takes the waiting branch: it awaits `CORE1.ready()` until B's `set()`
    // wakes it, then spawns REMOTE onto executor B.
    let sup = Supervisor::new(&GRAPH);
    START_ENTERED.store(true, Ordering::Release);
    sup.start(&spawner)
        .await
        .expect("start spawns REMOTE via the slot once B fills it");
    while !REMOTE_STARTED.load(Ordering::Acquire) {
        embassy_futures::yield_now().await;
    }
    assert!(REMOTE.is_running());
    assert_ne!(REMOTE.task_id(), 0, "the spawn adopted REMOTE's task id");

    sup.stop_node(&REMOTE).await.expect("stop_node");
    assert!(!REMOTE.is_running());

    DONE.store(true, Ordering::Release);
}

/// Holds the late slot publish until the driver is inside `start` and its
/// executor has run 50 more passes: `start` is then parked on the empty
/// slot, so the rendezvous branch is the one taken.
fn wait_until_start_parked() {
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !START_ENTERED.load(Ordering::Acquire) {
        assert!(
            StdInstant::now() < deadline,
            "the driver never entered start()"
        );
        std::thread::sleep(StdDuration::from_millis(1));
    }
    let seen = PASSES.load(Ordering::Acquire);
    while PASSES.load(Ordering::Acquire) < seen + 50 {
        assert!(
            StdInstant::now() < deadline,
            "the driver's executor stalled"
        );
        std::thread::sleep(StdDuration::from_millis(1));
    }
    PROBE_STOP.store(true, Ordering::SeqCst);
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

    set_core(0);
    trace::on_task_exec_begin(EXEC_C0, 71);
    clock.advance(Duration::from_ticks(10));
    set_core(1);
    trace::on_task_exec_begin(EXEC_C1, 72);
    clock.advance(Duration::from_ticks(20));
    set_core(0);
    trace::on_task_exec_end(EXEC_C0, 71);
    set_core(1);
    clock.advance(Duration::from_ticks(5));
    trace::on_task_exec_end(EXEC_C1, 72);

    assert_eq!(CA.exec_ticks(), 30, "core 0 poll exact (10 + 20 overlap)");
    assert_eq!(CB.exec_ticks(), 25, "core 1 poll exact (20 overlap + 5)");
    assert_eq!(CA.max_poll_ticks(), 30, "no cross-charge into core 0");
    assert_eq!(CB.max_poll_ticks(), 25, "no cross-charge into core 1");
    set_core(0);
    trace::on_task_exec_begin(EXEC_C0, 71);
    clock.advance(Duration::from_ticks(3));
    trace::on_task_exec_end(EXEC_C0, 71);
    assert_eq!(CA.exec_ticks(), 33, "no stolen residue on core 0");

    // Thread B ("core 1"): its executor's only supervisor-visible artifact is
    // the SendSpawner it publishes into the graph's CORE1 slot.
    std::thread::spawn(|| {
        wait_until_start_parked();
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        set_core(1);
        executor.run(|spawner| {
            CORE1.set(spawner.make_send());
        });
    });
    // Thread A ("core 0"): `init` runs on the executor's own thread.
    support::run(
        |spawner| {
            set_core(0);
            spawner.spawn(driver_task(spawner).unwrap());
            spawner.spawn(pass_probe().unwrap());
        },
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "cross-thread start/stop did not complete (REMOTE_STARTED = {})",
                REMOTE_STARTED.load(Ordering::Acquire)
            )
        },
    );
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    // The id is cleared by `on_task_end` on executor B after REMOTE's future
    // returns, which can trail the ack that ended `stop_node`.
    while REMOTE.task_id() != 0 {
        assert!(
            StdInstant::now() < deadline,
            "task_end never cleared REMOTE's id after the stop (id = {})",
            REMOTE.task_id()
        );
        std::thread::sleep(StdDuration::from_millis(1));
    }
}
