//! Cross-thread tests for the multi-core story: with the std
//! critical-section impl, two host threads running real embassy executors are a
//! faithful model of two cores — same atomics, same Signals, same SendSpawner
//! enqueue path the MCU cores would use.
//!
//! Covers, in one sequential `#[test]` (global state):
//!   1. per-core `trace-nested` stacks: concurrently-open polls on two "cores"
//!      (simulated by a switchable core-id fn) must not cross-charge — the bug
//!      a single LIFO stack would produce;
//!   2. the cross-core spawn rendezvous: `Supervisor::start` awaiting a node's
//!      `executor:` `SpawnerSlot` (`ready()`) that thread B fills ~50 ms late — the
//!      WAITING branch, standing in for core 1 publishing its `SendSpawner` after
//!      core 0 has already reached bring-up. Because the wait is now a real async
//!      `.await` (not the old `block_on` busy-spin, which starved the std CS mutex),
//!      it resolves cross-thread in-process, so this path is exercised here rather
//!      than being hardware-only;
//!   3. the full graph path across threads: `Supervisor::start` on executor A
//!      spawning an `executor: CORE1` node onto executor B through the slot,
//!      then `stop_node` driving the shutdown/ack handshake cross-thread.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph, trace};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    executor CORE1;
    node REMOTE = Terminate, deps: [], executor: CORE1, spawn: remote_task;
    // Parked nodes for the per-core nesting simulation (part 1).
    node CA = Pause, deps: [];
    node CB = Pause, deps: [];
}

/// Which "core" the fake core-id fn reports; flipped by the simulation.
static FAKE_CORE: AtomicUsize = AtomicUsize::new(0);
fn fake_core() -> usize {
    FAKE_CORE.load(Ordering::Relaxed)
}

static REMOTE_STARTED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

#[embassy_executor::task]
async fn remote_task(node: &'static TaskNode) {
    REMOTE_STARTED.store(true, Ordering::Release);
    node.wait_shutdown().await;
    node.ack_dropped();
}

/// Runs on executor A ("core 0"): rendezvous with B, start the graph (which
/// spawns REMOTE onto B through the CORE1 slot), then stop it cross-thread.
#[embassy_executor::task]
async fn driver_task(spawner: Spawner) {
    // (2) + (3) Start the graph WITHOUT pre-filling the slot: thread B publishes
    // CORE1's SendSpawner ~50 ms late (below), so `Supervisor::start` takes the
    // WAITING branch — it awaits `CORE1.ready()` internally (a real executor await,
    // not a busy-spin), yielding until B's `set()` wakes it, then spawns REMOTE onto
    // executor B through the slot. This is the cross-core rendezvous the firmware
    // relies on, now handled by the supervisor instead of an explicit app-side await.
    let sup = Supervisor::new(&GRAPH);
    sup.start(spawner)
        .await
        .expect("start spawns REMOTE via the slot once B fills it");
    // REMOTE runs on executor B (another thread): wait for its first poll.
    while !REMOTE_STARTED.load(Ordering::Acquire) {
        embassy_futures::yield_now().await;
    }
    assert!(REMOTE.is_running());

    // (3) cross-thread shutdown/ack handshake.
    sup.stop_node(&REMOTE).await;
    assert!(!REMOTE.is_running());

    DONE.store(true, Ordering::Release);
}

#[test]
fn multicore() {
    let clock = MockDriver::get();
    trace::register_graph(&GRAPH.nodes[..]);

    // ── (1) per-core nesting stacks ─────────────────────────────────────────
    // Two overlapping (NOT nested) polls on different cores: core 1's poll
    // opens while core 0's is mid-flight. With the single-core stack this
    // interleaving is a LIFO violation that cross-charges; with per-core
    // stacks both attributions stay exact (overlapping wall time counts in
    // both — correct for genuinely concurrent cores).
    trace::set_core_id_fn(fake_core);
    const EXEC_C0: u32 = 0xa0;
    const EXEC_C1: u32 = 0xa1;
    CA.set_task_id(71);
    CB.set_task_id(72);

    FAKE_CORE.store(0, Ordering::Relaxed);
    trace::on_task_exec_begin(EXEC_C0, 71);
    clock.advance(Duration::from_ticks(10));
    FAKE_CORE.store(1, Ordering::Relaxed);
    trace::on_task_exec_begin(EXEC_C1, 72); // "concurrent" poll on core 1
    clock.advance(Duration::from_ticks(20));
    FAKE_CORE.store(0, Ordering::Relaxed);
    trace::on_task_exec_end(EXEC_C0, 71); // ends while core 1 still polling
    FAKE_CORE.store(1, Ordering::Relaxed);
    clock.advance(Duration::from_ticks(5));
    trace::on_task_exec_end(EXEC_C1, 72);

    assert_eq!(CA.exec_ticks(), 30, "core 0 poll exact (10 + 20 overlap)");
    assert_eq!(CB.exec_ticks(), 25, "core 1 poll exact (20 overlap + 5)");
    assert_eq!(CA.max_poll_ticks(), 30, "no cross-charge into core 0");
    assert_eq!(CB.max_poll_ticks(), 25, "no cross-charge into core 1");
    // Stacks are clean afterwards: short follow-up polls stay exact.
    FAKE_CORE.store(0, Ordering::Relaxed);
    trace::on_task_exec_begin(EXEC_C0, 71);
    clock.advance(Duration::from_ticks(3));
    trace::on_task_exec_end(EXEC_C0, 71);
    assert_eq!(CA.exec_ticks(), 33, "no stolen residue on core 0");

    // All executor-thread hooks from here on map to core 0 (single stack each).
    FAKE_CORE.store(0, Ordering::Relaxed);

    // ── (2) + (3): two real executors on two threads ────────────────────────
    // Thread A: "core 0" — runs the driver (supervisor) task.
    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver_task(spawner).unwrap());
        });
    });
    // Thread B: "core 1" — its executor's only supervisor-visible artifact is
    // the SendSpawner it publishes into the graph's CORE1 slot, deliberately
    // late so `Supervisor::start`'s internal slot wait takes the WAITING path.
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
