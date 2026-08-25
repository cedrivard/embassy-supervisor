use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph, try_wait_health};
use embassy_time::{Duration, MockDriver};

pub static ESTIMATE: AtomicU32 = AtomicU32::new(0);
pub static SETPOINT: AtomicU32 = AtomicU32::new(0);
pub static NEVER_WRITTEN: AtomicU32 = AtomicU32::new(0);
pub static PRODUCED: AtomicU32 = AtomicU32::new(0);

fn publish(signal: &AtomicU32) {
    signal.fetch_add(1, Ordering::Relaxed);
}

supervisor_graph! {
    observe writes: it.load(core::sync::atomic::Ordering::Relaxed);
    observe reads:  it.load(core::sync::atomic::Ordering::Relaxed);

    node ESTIMATOR = Terminate, deps: [], beat_timeout: 100,
        writes: [crate::ESTIMATE observed beat];

    node LIAR = Terminate, deps: [], beat_timeout: 100,
        reads: [crate::ESTIMATE observed],
        writes: [crate::NEVER_WRITTEN observed beat];

    node HAND_BEATER = Terminate, deps: [], beat_timeout: 100,
        writes: [crate::SETPOINT observed];

    node PRODUCER = Terminate, deps: [], beat_timeout: 100, ready_on_write,
        writes: [crate::PRODUCED observed beat];
}

static SUP: Supervisor<4, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);
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

    // ── `ready_on_write`: readiness follows output, not a line of code ────
    // One settle sweep first: the monitor computes its period and parks on the
    // first poll, so nothing can be asserted about a sweep until it has run one.
    publish(&ESTIMATE);
    HAND_BEATER.beat();
    sweeps(clock, SWEEP, 1).await;
    assert!(
        !PRODUCER.is_ready(),
        "nothing has been produced yet, so nothing is ready"
    );

    publish(&PRODUCED);
    publish(&ESTIMATE);
    HAND_BEATER.beat();
    // A quarter of the beat budget. Readiness gates dependents against their
    // `slot_timeout`, so a node still waiting to assert is probed at a fraction
    // of its budget rather than only when it would go stale — noticing the
    // first write a whole budget late would spend someone else's bring-up time.
    sweeps(clock, Duration::from_millis(25), 1).await;
    assert!(
        PRODUCER.is_ready(),
        "the first advance asserts readiness well inside the 100ms budget"
    );

    for _ in 0..6 {
        publish(&ESTIMATE);
        publish(&PRODUCED);
        publish(&SETPOINT);
        HAND_BEATER.beat();
        sweeps(clock, SWEEP, 1).await;
    }

    let stale = stale_nodes();
    assert_eq!(
        stale,
        vec!["liar"],
        "a wrong declaration is a stale node, not a log line: {stale:?}"
    );

    publish(&SETPOINT);
    sweeps(clock, Duration::from_millis(150), 1).await;
    let stale = stale_nodes();
    assert!(
        stale.contains(&"estimator"),
        "the signal stopped, so the node reads as stale: {stale:?}"
    );
    assert!(
        stale.contains(&"hand-beater"),
        "its still-advancing `observed` (no `beat`) write is not a \
         heartbeat — only beat() kept it alive: {stale:?}"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn observed_writes_drive_liveness_and_readiness() {
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
