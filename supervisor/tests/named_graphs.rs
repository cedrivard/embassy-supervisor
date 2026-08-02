//! Named multi-graphs: a primary (unnamed) graph and a `name:`d sub-graph
//! coexist in one file; the sub-graph supervisor is start()/teardown()-cycled
//! per app phase (the subordinate-state-machine shape — start() resets each
//! node, so cycle 2+ workers start with a clean handle, and it is idempotent:
//! running nodes are skipped); a MIXED-mode sub-graph cycles too, because
//! start() resumes a Pause instance parked by the previous teardown instead of
//! double-spawning it (spawned once, resumed per re-entry); one driver task
//! applies a control command against BOTH supervisors and the foreign one
//! no-ops; the one-graph alternative (Activate-on-leaf / Deactivate-on-root
//! subtree cascades) is locked too. Harness as teardown.rs.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{ControlCommand, ControlOp, Supervisor, TaskNode, supervisor_graph};

// ── Primary graph (unnamed): the always-on app side, with a disabled subtree
//    exercising the Activate/Deactivate cascade pattern. ─────────────────────
supervisor_graph! {
    node MAIN = Terminate, deps: [], task: main_worker;
    // The one-graph subordinate pattern: Terminate + disabled, activated as a
    // subtree via control (leaf pulls deps up, root cascades dependents down).
    node WIFI = Terminate, deps: [], task: wifi_worker, disabled;
    node UPLOAD = Terminate, deps: [WIFI], task: upload_worker, disabled;
}

// ── Named sub-graph: a dedicated supervisor the state machine cycles with
//    whole-graph start()/teardown() calls. ──────────────────────────────────
supervisor_graph! {
    name: SCAN_GRAPH;
    node SENSOR = Terminate, deps: [], task: sensor_worker;
    node REPORT = Terminate, deps: [SENSOR], task: report_worker;
    // Mixed-mode cycling: parked by each teardown, RESUMED (not respawned) by
    // the next start().
    node GAUGE = Pause, deps: [], task: gauge_worker;
}

static MAIN_SPAWNS: AtomicU32 = AtomicU32::new(0);
static WIFI_SPAWNS: AtomicU32 = AtomicU32::new(0);
static UPLOAD_SPAWNS: AtomicU32 = AtomicU32::new(0);
static SENSOR_SPAWNS: AtomicU32 = AtomicU32::new(0);
static REPORT_SPAWNS: AtomicU32 = AtomicU32::new(0);
static GAUGE_SPAWNS: AtomicU32 = AtomicU32::new(0);
static GAUGE_RESUMES: AtomicU32 = AtomicU32::new(0);
/// Set if any sub-graph worker ever observed a latched shutdown flag at spawn —
/// the bug start()'s per-cycle reset exists to prevent.
static STALE_SHUTDOWN_SEEN: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn park(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn main_worker(node: &'static TaskNode) {
    MAIN_SPAWNS.fetch_add(1, Ordering::SeqCst);
    park(node).await;
}
async fn wifi_worker(node: &'static TaskNode) {
    WIFI_SPAWNS.fetch_add(1, Ordering::SeqCst);
    park(node).await;
}
async fn upload_worker(node: &'static TaskNode) {
    // First poll happens after WIFI's (activate brings deps up first).
    assert!(WIFI.is_running(), "leaf spawned after its dep");
    UPLOAD_SPAWNS.fetch_add(1, Ordering::SeqCst);
    park(node).await;
}
async fn sensor_worker(node: &'static TaskNode) {
    if node.shutdown_requested() {
        STALE_SHUTDOWN_SEEN.store(true, Ordering::SeqCst);
    }
    SENSOR_SPAWNS.fetch_add(1, Ordering::SeqCst);
    park(node).await;
}
/// The Pause protocol: ack each stop, park, count each resume — spawned once
/// for the whole test, resumed in place every cycle after the first.
async fn gauge_worker(node: &'static TaskNode) {
    GAUGE_SPAWNS.fetch_add(1, Ordering::SeqCst);
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
        GAUGE_RESUMES.fetch_add(1, Ordering::SeqCst);
    }
}

async fn report_worker(node: &'static TaskNode) {
    assert!(SENSOR.is_running(), "dep order inside the sub-graph");
    if node.shutdown_requested() {
        STALE_SHUTDOWN_SEEN.store(true, Ordering::SeqCst);
    }
    REPORT_SPAWNS.fetch_add(1, Ordering::SeqCst);
    park(node).await;
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
    let app = Supervisor::new(&GRAPH);
    let sub = Supervisor::new(&SCAN_GRAPH);

    app.start(spawner).await.expect("app start");
    settle(|| MAIN_SPAWNS.load(Ordering::SeqCst) == 1).await;
    assert!(
        !WIFI.is_running() && !UPLOAD.is_running(),
        "disabled at boot"
    );

    // ── Named sub-graph, cycled like the user's state machine: three full
    //    start()/teardown() cycles, dependency-ordered both ways each time. ──
    for cycle in 1..=3u32 {
        sub.start(spawner).await.expect("sub start");
        settle(|| REPORT_SPAWNS.load(Ordering::SeqCst) == cycle).await;
        assert!(SENSOR.is_running() && REPORT.is_running() && GAUGE.is_running());
        // Idempotent: a second start() in the running state changes nothing.
        sub.start(spawner).await.expect("sub start again");
        assert_eq!(SENSOR_SPAWNS.load(Ordering::SeqCst), cycle);
        assert_eq!(GAUGE_SPAWNS.load(Ordering::SeqCst), 1);
        sub.teardown().await.expect("sub teardown");
        assert!(!SENSOR.is_running() && !REPORT.is_running() && !GAUGE.is_running());
    }
    assert_eq!(SENSOR_SPAWNS.load(Ordering::SeqCst), 3);
    assert_eq!(
        (
            GAUGE_SPAWNS.load(Ordering::SeqCst),
            GAUGE_RESUMES.load(Ordering::SeqCst),
        ),
        (1, 2),
        "the Pause node was spawned once and RESUMED on each re-entry"
    );
    assert!(
        !STALE_SHUTDOWN_SEEN.load(Ordering::SeqCst),
        "start() reset each node — no worker saw a latched shutdown flag"
    );
    // The app graph was untouched by the sub-graph cycling.
    assert!(MAIN.is_running());
    assert_eq!(MAIN_SPAWNS.load(Ordering::SeqCst), 1);

    // ── Single-node pause/resume on the Pause node: stop_node IS the pause
    //    (ack + park), resume_node the symmetric other half; a Terminate node
    //    is untouched by resume_node. ─────────────────────────────────────────
    sub.start(spawner).await.expect("sub start for pause test");
    settle(|| GAUGE.is_running()).await; // start() resumed it: resumes = 3
    sub.stop_node(&GAUGE).await.expect("pause = stop_node");
    assert!(!GAUGE.is_running());
    sub.resume_node(&SENSOR); // wrong mode: no-op
    assert_eq!(GAUGE_RESUMES.load(Ordering::SeqCst), 3, "not resumed yet");
    sub.resume_node(&GAUGE);
    settle(|| GAUGE_RESUMES.load(Ordering::SeqCst) == 4).await;
    assert!(GAUGE.is_running(), "resumed in place, single node");
    assert_eq!(GAUGE_SPAWNS.load(Ordering::SeqCst), 1, "still one instance");
    sub.teardown().await.expect("clean up pause test");

    // ── One driver, two supervisors: apply each command to both; the foreign
    //    supervisor no-ops (seed finds no index). ─────────────────────────────
    let cmd = ControlCommand {
        node: &UPLOAD,
        op: ControlOp::Activate,
    };
    sub.apply_control(cmd, spawner)
        .await
        .expect("foreign no-op");
    assert!(
        !UPLOAD.is_running() && !WIFI.is_running(),
        "the sub-graph supervisor cannot see the app graph's nodes"
    );
    app.activate(&UPLOAD, spawner).await; // the direct shorthand
    settle(|| UPLOAD_SPAWNS.load(Ordering::SeqCst) == 1).await;
    assert!(
        WIFI.is_running() && UPLOAD.is_running(),
        "Activate on the LEAF pulled its dep up, dependency-ordered"
    );

    // Deactivate on the ROOT cascades the dependent down first (reverse order).
    app.deactivate(&WIFI).await.expect("deactivate");
    assert!(!UPLOAD.is_running() && !WIFI.is_running());
    assert!(MAIN.is_running(), "unrelated node untouched by the cascade");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn named_subgraph_cycles_and_shared_control() {
    let _clock = embassy_time::MockDriver::get();

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
            "did not complete (sensor={}, upload={})",
            SENSOR_SPAWNS.load(Ordering::SeqCst),
            UPLOAD_SPAWNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
