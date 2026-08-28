use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{HealthKind, Supervisor, supervisor_graph, try_wait_health};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node BEATER = Terminate, deps: [], #[cfg(all())] beat_timeout: 100;
    node JITTERY = Terminate, deps: [], beat_timeout: 100, beat_window: 3;
    node UNPOLICED = Terminate, deps: [], #[cfg(any())] beat_timeout: 1;
}

static SUP: Supervisor<3, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);
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

fn drain() -> Vec<(&'static str, HealthKind)> {
    let mut out = Vec::new();
    while let Some(ev) = try_wait_health() {
        out.push((ev.node.name(), ev.kind));
    }
    out
}

fn stales(events: &[(&'static str, HealthKind)], name: &str) -> usize {
    events
        .iter()
        .filter(|(n, k)| *n == name && matches!(k, HealthKind::Stale { .. }))
        .count()
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    spawner.spawn(monitor_task().unwrap());

    sweeps(clock, SWEEP, 4).await;
    assert!(drain().is_empty(), "no events before the graph is up");

    SUP.start(&spawner).await.expect("start");
    assert!(BEATER.is_running() && JITTERY.is_running() && UNPOLICED.is_running());

    for _ in 0..6 {
        BEATER.beat();
        JITTERY.beat();
        sweeps(clock, SWEEP, 1).await;
    }
    assert!(
        drain().is_empty(),
        "a node that keeps beating produces no events"
    );

    sweeps(clock, Duration::from_millis(150), 2).await;
    let events = drain();
    assert_eq!(
        events.len(),
        1,
        "only the window-1 node reports on its first miss, got {events:?}"
    );
    assert_eq!(events[0].0, "beater");
    assert!(matches!(events[0].1, HealthKind::Stale { .. }));

    sweeps(clock, SWEEP, 1).await;
    let events = drain();
    assert!(
        events.is_empty(),
        "a still-stale node is not re-reported, and a window is not yet met: {events:?}"
    );

    sweeps(clock, SWEEP, 1).await;
    let events = drain();
    assert_eq!(
        stales(&events, "jittery"),
        1,
        "the windowed node reports after its consecutive misses, got {events:?}"
    );

    sweeps(clock, Duration::from_secs(30), 3).await;
    let events = drain();
    assert!(
        !events.iter().any(|(n, _)| *n == "unpoliced"),
        "a node whose beat_timeout: is cfg'd out is never policed, got {events:?}"
    );

    assert!(
        BEATER.is_running(),
        "the monitor never stops a node it reports"
    );
    assert!(!BEATER.is_disabled(), "and never disables one");

    BEATER.beat();
    sweeps(clock, SWEEP, 1).await;
    let events = drain();
    assert!(
        events
            .iter()
            .any(|(n, k)| *n == "beater" && *k == HealthKind::Recovered),
        "a node that beats again is reported recovered, got {events:?}"
    );

    sweeps(clock, Duration::from_millis(200), 1).await;
    let events = drain();
    assert_eq!(
        stales(&events, "beater"),
        1,
        "a recovered node can trip again, got {events:?}"
    );

    BEATER.mark_exited();
    assert!(!BEATER.is_running());
    let _ = drain();
    sweeps(clock, Duration::from_secs(10), 3).await;
    let events = drain();
    assert!(
        !events.iter().any(|(n, _)| *n == "beater"),
        "a stopped node is down, not stale, got {events:?}"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn polices_only_declared_nodes_and_only_reports() {
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
