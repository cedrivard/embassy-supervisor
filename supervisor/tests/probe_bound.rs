//! A `ready_on_write` node that never asserts drives the monitor at its capped
//! probe rate only for its activation's first 100 ms. After that the sweep
//! runs at the other nodes' own cadence, so a stalled node's `beat_window`
//! fills at its own overdue rate rather than at 12.5 ms.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{HealthKind, Supervisor, TaskNode, supervisor_graph, try_wait_health};
use embassy_time::{Duration, MockDriver};

pub static OUT: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

supervisor_graph! {
    observe writes: it.load(core::sync::atomic::Ordering::Relaxed);
    node SILENT = Terminate, deps: [], task: silent, beat_timeout: 1000, ready_on_write,
        writes: [crate::OUT observed beat];
    node STALLER = Terminate, deps: [], task: staller, beat_timeout: 100, beat_window: 8;
}

static SUP: Supervisor<2, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);

/// Never writes `OUT`, so it never asserts.
async fn silent(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

/// Beats once, then stalls.
async fn staller(node: &'static TaskNode) {
    node.beat();
    node.wait_shutdown().await;
    node.mark_exited();
}

#[embassy_executor::task]
async fn monitor_task() {
    SUP.monitor().await;
}

async fn settle() {
    for _ in 0..4 {
        embassy_futures::yield_now().await;
    }
}

/// `n` steps of `step`. The monitor's next check is always at least a tick
/// out and the clock moves only here, so each step runs at most one sweep.
async fn sweeps(clock: &MockDriver, step: Duration, n: usize) {
    for _ in 0..n {
        clock.advance(step);
        settle().await;
    }
}

fn drain() -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    while let Some(ev) = try_wait_health() {
        let kind = match ev.kind {
            HealthKind::Stale { .. } => "stale",
            HealthKind::Recovered => "recovered",
            _ => "other",
        };
        out.push((ev.node.name(), kind));
    }
    out
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    SUP.start(&spawner).await.expect("start");
    // Both bodies run before the monitor plans its first sleep, so STALLER's
    // beat is granted at t = 0 and every check below is a whole step.
    settle().await;
    spawner.spawn(monitor_task().unwrap());
    settle().await;

    // STALLER is stale from 125 ms. Probed at 12.5 ms for as long as SILENT
    // is unready, every 25 ms step would sweep and strike, reaching the
    // eighth strike at 300 ms. SILENT's probe cap ends at 100 ms, so the
    // sweep follows STALLER's own overdue cadence of 50 ms instead.
    sweeps(clock, Duration::from_millis(25), 14).await;
    assert!(
        drain().is_empty(),
        "at 350 ms STALLER has 5 strikes of 8, not a full window"
    );

    // Strikes at 125, 175, ..., 475 ms: the eighth reports.
    sweeps(clock, Duration::from_millis(25), 12).await;
    assert_eq!(drain(), [("staller", "stale")]);
    assert!(!SILENT.is_ready(), "SILENT never wrote");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn an_unready_probe_stops_driving_the_sweep_after_its_first_100ms() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
}
