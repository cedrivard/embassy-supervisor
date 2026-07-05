//! Behavioral tests for the async `teardown` / `respawn_terminate` paths, focused on
//! **detached** nodes (self-managed: brought up once by `start`, then skipped by
//! teardown *and* respawn). A single real embassy executor on a std thread drives a
//! driver task through `start -> teardown -> respawn_terminate`, asserting the node
//! lifecycle transitions at each step.
//!
//! As in `multicore.rs`, the std critical-section impl plus a real executor make this
//! a faithful in-process model of the on-device scheduler (same atomics, `Signal`s,
//! and shutdown/ack handshake the MCU would run).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

supervisor_graph! {
    node NORMAL = Terminate, deps: [], spawn: normal_task;
    node PAUSED = Pause, deps: [], spawn: paused_task;
    node DAEMON = Terminate, deps: [], spawn: daemon_task;
}

static NORMAL_SPAWNS: AtomicU32 = AtomicU32::new(0);
static PAUSED_SPAWNS: AtomicU32 = AtomicU32::new(0);
static PAUSED_RESUMES: AtomicU32 = AtomicU32::new(0);
static DAEMON_SPAWNS: AtomicU32 = AtomicU32::new(0);
/// Set only if the supervisor wrongly signals the detached daemon to shut down.
static DAEMON_SHUTDOWN_SEEN: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// A normal Terminate node: counts each (re)spawn, then on shutdown acks and exits,
/// so the supervisor respawns it on the next bring-up. `pool_size = 2` leaves a free
/// slot for the respawn while the original instance is still draining.
#[embassy_executor::task(pool_size = 2)]
async fn normal_task(node: &'static TaskNode) {
    NORMAL_SPAWNS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
}

/// A Pause node: counts its single spawn, then loops the pause protocol — ack a
/// shutdown, park on `wait_resume`, and count each resume. It is *resumed in place*,
/// never respawned, so its spawn count stays 1 across a teardown/resume cycle.
#[embassy_executor::task]
async fn paused_task(node: &'static TaskNode) {
    PAUSED_SPAWNS.fetch_add(1, Ordering::SeqCst);
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
        PAUSED_RESUMES.fetch_add(1, Ordering::SeqCst);
    }
}

/// A detached (self-managed) node: marks itself detached, counts its single spawn,
/// then parks on `wait_shutdown` forever. The supervisor must never signal it, so the
/// line past the await must never run.
#[embassy_executor::task]
async fn daemon_task(node: &'static TaskNode) {
    node.set_detached(true);
    DAEMON_SPAWNS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    // Reached only if the supervisor wrongly tore the detached node down.
    DAEMON_SHUTDOWN_SEEN.store(true, Ordering::SeqCst);
}

/// Yield until `f()` holds (freshly-spawned tasks need to be polled before their
/// bodies have run) or a generous turn budget elapses.
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

    // ── start(): all three nodes come up; the daemon detaches itself ─────────
    sup.start(spawner).await.expect("start");
    settle(|| {
        NORMAL_SPAWNS.load(Ordering::SeqCst) == 1
            && PAUSED_SPAWNS.load(Ordering::SeqCst) == 1
            && DAEMON_SPAWNS.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(NORMAL.is_running(), "normal running after start");
    assert!(PAUSED.is_running(), "paused running after start");
    assert!(DAEMON.is_running(), "daemon running after start");
    assert!(DAEMON.is_detached(), "daemon marked itself detached");
    assert!(!NORMAL.is_detached(), "normal is not detached");
    PHASE.store(1, Ordering::SeqCst);

    // ── teardown(): tears down NORMAL + PAUSED, skips the detached DAEMON ─────
    sup.teardown().await;
    assert!(!NORMAL.is_running(), "normal torn down");
    assert!(!PAUSED.is_running(), "paused parked (torn down) by teardown");
    assert!(DAEMON.is_running(), "detached daemon survives teardown");
    assert!(
        !DAEMON_SHUTDOWN_SEEN.load(Ordering::SeqCst),
        "detached daemon was never signaled to shut down"
    );
    PHASE.store(2, Ordering::SeqCst);

    // ── resume_pausable(): resumes PAUSED in place (not respawned) ───────────
    sup.resume_pausable();
    settle(|| PAUSED_RESUMES.load(Ordering::SeqCst) == 1).await;
    assert!(PAUSED.is_running(), "paused resumed");
    assert_eq!(
        PAUSED_SPAWNS.load(Ordering::SeqCst),
        1,
        "paused resumed in place — not respawned"
    );
    PHASE.store(3, Ordering::SeqCst);

    // ── respawn_terminate(): respawns NORMAL, skips PAUSED and detached DAEMON ─
    sup.respawn_terminate(spawner).await.expect("respawn");
    settle(|| NORMAL_SPAWNS.load(Ordering::SeqCst) == 2).await;
    assert!(NORMAL.is_running(), "normal respawned");
    assert_eq!(
        PAUSED_SPAWNS.load(Ordering::SeqCst),
        1,
        "respawn_terminate leaves the Pause node alone"
    );
    assert_eq!(
        NORMAL_SPAWNS.load(Ordering::SeqCst),
        2,
        "normal spawned exactly twice (once at start, once at respawn)"
    );
    assert_eq!(
        DAEMON_SPAWNS.load(Ordering::SeqCst),
        1,
        "detached daemon NOT respawned — no double-spawn of the still-running instance"
    );
    assert!(
        DAEMON.is_running() && DAEMON.is_detached(),
        "daemon is still the same running, detached instance"
    );
    assert!(!DAEMON_SHUTDOWN_SEEN.load(Ordering::SeqCst));

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn detached_survives_teardown_and_respawn() {
    // embassy-time driver: teardown's ack-timeout `Timer` needs a registered driver
    // even though it never fires (NORMAL acks long before the frozen clock advances).
    let _clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    // The executor thread never exits; poll the completion flag with a deadline.
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (phase={}, normal_spawns={}, daemon_spawns={}, daemon_shutdown_seen={})",
            PHASE.load(Ordering::SeqCst),
            NORMAL_SPAWNS.load(Ordering::SeqCst),
            DAEMON_SPAWNS.load(Ordering::SeqCst),
            DAEMON_SHUTDOWN_SEEN.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
