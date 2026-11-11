//! The health mailbox re-reports a transition that found it full: the next
//! sweep sends it again, a transition reversed before it could be delivered
//! is dropped together with its reversal, and a stop ends a stall without an
//! event.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{HealthKind, Supervisor, TaskNode, supervisor_graph, try_wait_health};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node N0 = Terminate, deps: [], beat_timeout: 100;
    node N1 = Terminate, deps: [], beat_timeout: 100;
    node N2 = Terminate, deps: [], beat_timeout: 100;
    node N3 = Terminate, deps: [], beat_timeout: 100;
    node N4 = Terminate, deps: [], beat_timeout: 100;
    node N5 = Terminate, deps: [], beat_timeout: 100;
}

static SUP: Supervisor<6, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);
static ALL: [&TaskNode; 6] = [&N0, &N1, &N2, &N3, &N4, &N5];
static DONE: AtomicBool = AtomicBool::new(false);

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

fn beat(nodes: &[&TaskNode]) {
    for n in nodes {
        n.beat();
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
    spawner.spawn(monitor_task().unwrap());

    for _ in 0..3 {
        beat(&ALL);
        sweeps(clock, Duration::from_millis(100), 1).await;
    }
    assert!(drain().is_empty(), "every node beat: no events");

    // Six stalls at once into a four-slot mailbox: two transitions wait. Which
    // four get in is the sweep's order; the rest of the scenario follows the
    // two left out, whichever they are.
    sweeps(clock, Duration::from_millis(150), 1).await;
    let mut first = drain();
    first.sort();
    assert_eq!(first.len(), 4);
    assert!(first.iter().all(|(_, k)| *k == "stale"));
    let left_out: Vec<&TaskNode> = ALL
        .iter()
        .copied()
        .filter(|n| !first.iter().any(|(name, _)| *name == n.name()))
        .collect();
    assert_eq!(left_out.len(), 2);

    // The next sweep sends `left_out[0]`'s Stale again. `left_out[1]` beat in
    // between: its Stale and the Recovered that would follow it are both
    // dropped.
    left_out[1].beat();
    sweeps(clock, Duration::from_millis(60), 1).await;
    assert_eq!(
        drain(),
        [(left_out[0].name(), "stale")],
        "a dropped Stale comes late, not never"
    );

    // Five Recovered into four slots: the sweep's order lets the same four in,
    // and `left_out[0]`'s waits.
    beat(&ALL);
    sweeps(clock, Duration::from_millis(60), 1).await;
    let mut recovered = drain();
    recovered.sort();
    assert_eq!(
        recovered,
        first
            .iter()
            .map(|&(name, _)| (name, "recovered"))
            .collect::<Vec<_>>()
    );

    // A node that just beat is next checked one tick past its budget.
    beat(&ALL);
    sweeps(clock, Duration::from_millis(101), 1).await;
    assert_eq!(
        drain(),
        [(left_out[0].name(), "recovered")],
        "and so does a dropped Recovered"
    );

    beat(&ALL[1..]);
    sweeps(clock, Duration::from_millis(150), 1).await;
    assert_eq!(drain(), [("n0", "stale")]);

    N0.mark_exited();
    beat(&ALL[1..]);
    sweeps(clock, Duration::from_millis(60), 1).await;
    assert!(drain().is_empty(), "a stop is not an event");

    SUP.start(&spawner).await.expect("n0 starts again");
    beat(&ALL);
    sweeps(clock, Duration::from_millis(100), 1).await;
    assert!(
        drain().is_empty(),
        "no Recovered for a stall the stop ended"
    );

    beat(&ALL[1..]);
    sweeps(clock, Duration::from_millis(150), 1).await;
    assert_eq!(drain(), [("n0", "stale")], "a fresh stall");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_full_mailbox_delays_events_but_never_loses_them() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
}
