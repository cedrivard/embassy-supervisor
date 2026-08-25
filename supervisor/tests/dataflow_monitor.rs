use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, dataflow, supervisor_graph, try_wait_health};
use embassy_time::{Duration, MockDriver};

pub static OUT: AtomicU32 = AtomicU32::new(0);
pub static SCRATCH: AtomicU32 = AtomicU32::new(0);

#[dataflow]
async fn scout_task(node: &'static TaskNode) {
    node.put(&crate::SCRATCH, 0);
    core::future::pending::<()>().await;
}

/// The heartbeat write: the verb says this publish is the node's sign of life.
#[dataflow]
fn pulse_out(node: &'static TaskNode, v: u32) {
    node.beat_put(&crate::OUT, v);
}

#[dataflow]
fn pulse_scratch(node: &'static TaskNode, v: u32) {
    node.put(&crate::SCRATCH, v);
}

supervisor_graph! {
    node BEATER = Terminate, deps: [], beat_timeout: 100,
        writes: [crate::OUT];

    node SCOUT = Terminate, deps: [], beat_timeout: 100, task: scout_task, discover;
}

static SUP: Supervisor<2, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);
static DONE: AtomicBool = AtomicBool::new(false);

const SWEEP: Duration = Duration::from_millis(60);

#[embassy_executor::task]
async fn monitor_task() {
    SUP.monitor().await;
}

async fn sweeps(clock: &MockDriver, step: Duration, n: usize) {
    for _ in 0..n {
        clock.advance(step);
        for _ in 0..4 {
            embassy_futures::yield_now().await;
        }
    }
}

fn stale_nodes() -> Vec<&'static str> {
    let mut out = Vec::new();
    while let Some(ev) = try_wait_health() {
        if matches!(ev.kind, embassy_supervisor::HealthKind::Stale { .. }) {
            out.push(ev.node.name());
        }
    }
    out
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    spawner.spawn(monitor_task().unwrap());
    SUP.start(&spawner).await.expect("start");

    // Settle sweep: the monitor computes its period and parks on the first
    // poll, so nothing can be asserted about a sweep until it has run one.
    pulse_out(&BEATER, 0);
    pulse_scratch(&SCOUT, 0);
    sweeps(clock, SWEEP, 1).await;

    // ── The verb is what makes a write a heartbeat ───────────────────────
    // Neither body calls beat(). Over six sweeps spanning well past the
    // budget, BEATER's flag→check conversion keeps it alive, while SCOUT is
    for i in 1..=6 {
        pulse_out(&BEATER, i);
        pulse_scratch(&SCOUT, i);
        sweeps(clock, SWEEP, 1).await;
    }
    let stale = stale_nodes();
    assert_eq!(
        stale,
        vec!["scout"],
        "`beat_put` lives, `put` never claimed to: {stale:?}"
    );

    sweeps(clock, Duration::from_millis(150), 2).await;
    let stale = stale_nodes();
    assert!(
        stale.contains(&"beater"),
        "a quiet node goes stale exactly as before: {stale:?}"
    );

    pulse_out(&BEATER, 7);
    sweeps(clock, SWEEP, 2).await;
    let mut recovered = Vec::new();
    while let Some(ev) = try_wait_health() {
        if matches!(ev.kind, embassy_supervisor::HealthKind::Recovered) {
            recovered.push(ev.node.name());
        }
    }
    assert!(recovered.contains(&"beater"), "{recovered:?}");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn the_sweep_grants_the_beats() {
    let _clock = MockDriver::get();

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
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
