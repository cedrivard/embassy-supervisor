//! Behavioral tests for the async `teardown` / `respawn_terminate` / control-stop
//! paths and the **detached** flag — the supervisor starts a detached node once, then
//! stops managing it entirely. Two flavours: a long-lived daemon that parks on
//! `wait_shutdown`, and a self-managed **one-shot** (`deps: [NORMAL]`) that detaches,
//! runs once, and *exits*. Both must be skipped by teardown, respawn, *and* the
//! deactivate cascade. A single real embassy executor on a std thread drives a driver
//! task through `start -> teardown -> resume -> respawn -> deactivate`, asserting the
//! lifecycle at each step.
//!
//! The detached one-shot is the regression guard for the on-device panic: a Terminate
//! node that returns leaves a stale `is_running == true` (no ack), so if the deactivate
//! cascade pulled it in it would wait forever on an ack that never comes. Detaching (for
//! a node whose `deps:` are only start-ordering) opts it out of the cascade entirely.
//!
//! As in `multicore.rs`, the std critical-section impl plus a real executor make this
//! a faithful in-process model of the on-device scheduler (same atomics, `Signal`s,
//! and shutdown/ack handshake the MCU would run).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlOp, Supervisor, TaskNode, request_control, supervisor_graph, wait_control,
};
use embassy_time::MockDriver;

supervisor_graph! {
    node NORMAL = Terminate, deps: [], spawn: normal_task;
    node PAUSED = Pause, deps: [], spawn: paused_task;
    node DAEMON = Terminate, deps: [], spawn: daemon_task;
    node ONESHOT = Terminate, deps: [NORMAL], spawn: oneshot_task;
}

static NORMAL_SPAWNS: AtomicU32 = AtomicU32::new(0);
static PAUSED_SPAWNS: AtomicU32 = AtomicU32::new(0);
static PAUSED_RESUMES: AtomicU32 = AtomicU32::new(0);
static DAEMON_SPAWNS: AtomicU32 = AtomicU32::new(0);
/// Set only if the supervisor wrongly signals the detached daemon to shut down.
static DAEMON_SHUTDOWN_SEEN: AtomicBool = AtomicBool::new(false);
/// The self-managed one-shot's run count — must stay 1 (never respawned / re-run).
static ONESHOT_SPAWNS: AtomicU32 = AtomicU32::new(0);
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

/// A self-managed one-shot: marks itself `detached`, records its single run, and
/// *exits*. Because it returns without ever acking, its `is_running` stays a stale
/// `true` — so the supervisor must leave it alone. Detaching does that: teardown,
/// respawn, and the deactivate cascade all skip a detached node. If any stop path
/// signalled it, the ack would never come and, with the clock frozen, the driver would
/// hang forever, failing the test's deadline.
#[embassy_executor::task]
async fn oneshot_task(node: &'static TaskNode) {
    node.set_detached(true);
    ONESHOT_SPAWNS.fetch_add(1, Ordering::SeqCst);
    // Returns immediately — no wait_shutdown / ack_dropped.
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

    // ── start(): all nodes come up; the daemon detaches itself, the one-shot
    //    disables itself and exits ──────────────────────────────────────────────
    sup.start(spawner).await.expect("start");
    settle(|| {
        NORMAL_SPAWNS.load(Ordering::SeqCst) == 1
            && PAUSED_SPAWNS.load(Ordering::SeqCst) == 1
            && DAEMON_SPAWNS.load(Ordering::SeqCst) == 1
            && ONESHOT_SPAWNS.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(NORMAL.is_running(), "normal running after start");
    assert!(PAUSED.is_running(), "paused running after start");
    assert!(DAEMON.is_running(), "daemon running after start");
    assert!(DAEMON.is_detached(), "daemon marked itself detached");
    assert!(!NORMAL.is_detached(), "normal is not detached");
    assert!(
        ONESHOT.is_detached(),
        "one-shot marked itself detached and exited"
    );
    PHASE.store(1, Ordering::SeqCst);

    // ── teardown(): tears down NORMAL + PAUSED, skips the detached DAEMON ─────
    sup.teardown().await;
    assert!(!NORMAL.is_running(), "normal torn down");
    assert!(
        !PAUSED.is_running(),
        "paused parked (torn down) by teardown"
    );
    assert!(DAEMON.is_running(), "detached daemon survives teardown");
    assert!(
        !DAEMON_SHUTDOWN_SEEN.load(Ordering::SeqCst),
        "detached daemon was never signaled to shut down"
    );
    // The detached one-shot already exited (stale is_running); teardown must skip it.
    // Reaching this line at all proves it did — else the frozen-clock ack wait hangs.
    assert!(
        ONESHOT.is_detached(),
        "detached one-shot skipped by teardown"
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
    assert_eq!(
        ONESHOT_SPAWNS.load(Ordering::SeqCst),
        1,
        "detached one-shot NOT respawned by respawn_terminate"
    );
    PHASE.store(4, Ordering::SeqCst);

    // ── control deactivate cascade (the exact on-device panic path): stop NORMAL;
    //    its dependent ONESHOT is `detached` — a one-shot that already exited — so the
    //    cascade's growth loop skips it, never awaiting an ack it can't give ──────────
    request_control(&NORMAL, ControlOp::Deactivate);
    let cmd = wait_control().await;
    sup.apply_control(cmd, spawner).await;
    assert!(!NORMAL.is_running(), "normal control-stopped");
    assert!(NORMAL.is_disabled(), "normal marked disabled by deactivate");
    assert!(
        ONESHOT.is_detached(),
        "detached one-shot skipped by the deactivate cascade (did not hang on its ack)"
    );
    assert_eq!(
        ONESHOT_SPAWNS.load(Ordering::SeqCst),
        1,
        "one-shot ran exactly once across the whole lifecycle"
    );

    // ── deactivate the detached one-shot *directly*: it is seeded into the set,
    //    bypassing the growth-loop skip, so only the teardown loop's own detached guard
    //    prevents signalling a shutdown to the already-exited task. A no-op: it stays
    //    detached, is not even marked disabled, and the driver does not hang on an ack.
    request_control(&ONESHOT, ControlOp::Deactivate);
    let cmd = wait_control().await;
    sup.apply_control(cmd, spawner).await;
    assert!(
        ONESHOT.is_detached(),
        "detached one-shot still detached after direct deactivate"
    );
    assert!(
        !ONESHOT.is_disabled(),
        "detached one-shot left untouched by deactivate (not even disabled)"
    );
    assert_eq!(
        ONESHOT_SPAWNS.load(Ordering::SeqCst),
        1,
        "one-shot still ran exactly once"
    );

    // ── activate the detached one-shot directly: its `deps: [NORMAL]` edge is start-
    //    ordering only, so activate's growth loop must not expand from the detached
    //    target — else NORMAL (independently control-stopped just above) would be
    //    un-disabled and force-started. Expected: a complete no-op — the detached
    //    target is skipped by the bring-up loop, and the disabled dep is left alone.
    request_control(&ONESHOT, ControlOp::Activate);
    let cmd = wait_control().await;
    sup.apply_control(cmd, spawner).await;
    assert!(
        NORMAL.is_disabled(),
        "disabled dep NOT re-enabled by Activate(detached target)"
    );
    assert!(
        !NORMAL.is_running(),
        "disabled dep NOT restarted by Activate(detached target)"
    );
    assert_eq!(
        ONESHOT_SPAWNS.load(Ordering::SeqCst),
        1,
        "detached one-shot not respawned by Activate"
    );

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
