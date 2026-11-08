use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    CrumbPhase, FaultKind, Supervisor, TaskNode, crumb_word, decode_crumb, supervisor_graph,
};
use embassy_time::MockDriver;

supervisor_graph! {
    node A = Terminate, deps: [], task: plain;
    node B = Terminate, deps: [A], task: plain;
    node GATED = Terminate, deps: [B], task: gated,
        resources: [GATED_SLOT: u32], slot_timeout: 500;
}

// A second graph, driven by its own supervisor: it parks and resumes PARK,
// holds DEAF's stop on a missed ack, and drives B, which it does not contain.
supervisor_graph! {
    name: AUX;
    node PARK = Pause, deps: [], task: park;
    node DEAF = Terminate, deps: [], task: deaf, ack_timeout: 200;
}

static DONE: AtomicBool = AtomicBool::new(false);
/// Set once the bring-up has faulted: the test thread holds the clock again.
static START_DONE: AtomicBool = AtomicBool::new(false);

async fn plain(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn gated(node: &'static TaskNode, _slot: &mut u32) {
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn park(node: &'static TaskNode) {
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
    }
}

/// Never acks.
async fn deaf(_node: &'static TaskNode) {
    core::future::pending::<()>().await;
}

fn slot(node: &'static TaskNode) -> u16 {
    u16::from(GRAPH.index_of(node).expect("node is in GRAPH"))
}

fn aux_slot(node: &'static TaskNode) -> u16 {
    u16::from(AUX.index_of(node).expect("node is in AUX"))
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    assert_eq!(crumb_word(), 0, "no transition yet");
    assert_eq!(decode_crumb(crumb_word()), None);
    assert_eq!(A.crumb(), CrumbPhase::Idle);

    // GATED's slot is never provided: the wave waits on it (the test thread
    // watches its crumb read `Spawning`), then faults it at the deadline.
    let err = sup
        .start(&spawner)
        .await
        .expect_err("GATED's slot is empty");
    START_DONE.store(true, Ordering::SeqCst);
    assert!(matches!(err.kind, FaultKind::ResourceMissing));
    assert_eq!(A.crumb(), CrumbPhase::Started);
    assert_eq!(B.crumb(), CrumbPhase::Started);
    assert_eq!(GATED.crumb(), CrumbPhase::Faulted);
    assert_eq!(
        decode_crumb(crumb_word()),
        Some((CrumbPhase::Faulted, slot(&GATED))),
        "the word names the node and phase of the last transition"
    );

    sup.stop_node(&B).await.expect("stop B");
    assert_eq!(B.crumb(), CrumbPhase::Stopped);
    assert_eq!(A.crumb(), CrumbPhase::Started, "only B moved");
    assert_eq!(
        decode_crumb(crumb_word()),
        Some((CrumbPhase::Stopped, slot(&B)))
    );

    sup.start_node(&B, &spawner).await.expect("start B");
    assert_eq!(B.crumb(), CrumbPhase::Started);
    assert_eq!(
        decode_crumb(crumb_word()),
        Some((CrumbPhase::Started, slot(&B))),
        "a single-node verb resolves the slot index too"
    );

    assert_eq!(decode_crumb(0x0002_0001), None, "bit 31 clear: not a crumb");
    assert_eq!(decode_crumb(0x8063_0001), None, "unknown phase");

    let aux = Supervisor::new(&AUX);
    aux.start(&spawner).await.expect("aux start");
    aux.stop_node(&PARK).await.expect("park acks");
    aux.resume_node(&PARK);
    assert_eq!(PARK.crumb(), CrumbPhase::Resumed);
    assert_eq!(
        decode_crumb(crumb_word()),
        Some((CrumbPhase::Resumed, aux_slot(&PARK))),
        "a resume in place is its own phase, indexed in its own graph"
    );

    aux.stop_node(&B).await.expect("B acks");
    assert_eq!(
        decode_crumb(crumb_word()),
        Some((CrumbPhase::Stopped, u16::MAX)),
        "a node driven through a supervisor whose graph lacks it has no slot"
    );

    let err = aux.stop_node(&DEAF).await.expect_err("DEAF never acks");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout));
    assert_eq!(DEAF.crumb(), CrumbPhase::Faulted);
    assert_eq!(
        decode_crumb(crumb_word()),
        Some((CrumbPhase::Faulted, aux_slot(&DEAF)))
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn crumbs_track_the_last_phase_per_node_and_globally() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    // Hold the clock until the wave is parked on GATED's gate: a hang there is
    // exactly what the crumb has to name.
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while GATED.crumb() != CrumbPhase::Spawning {
        assert!(StdInstant::now() < deadline, "GATED never reached Spawning");
        std::thread::sleep(StdDuration::from_millis(1));
    }
    assert_eq!(
        decode_crumb(crumb_word()),
        Some((CrumbPhase::Spawning, slot(&GATED))),
        "a wave stuck on a gate leaves `Spawning` as the last word"
    );

    // Run the clock until the bring-up has faulted, then hold it: the phases
    // after it need no time, and DEAF's stop must be seen parked.
    while !START_DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "the bring-up never faulted");
        clock.advance(embassy_time::Duration::from_millis(50));
        std::thread::sleep(StdDuration::from_millis(2));
    }
    while DEAF.crumb() != CrumbPhase::StopSignalled {
        assert!(
            StdInstant::now() < deadline,
            "DEAF never reached StopSignalled"
        );
        std::thread::sleep(StdDuration::from_millis(1));
    }
    assert_eq!(
        decode_crumb(crumb_word()),
        Some((CrumbPhase::StopSignalled, aux_slot(&DEAF))),
        "a stop stuck on its ack leaves `StopSignalled` as the last word"
    );
    while !DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "did not complete");
        clock.advance(embassy_time::Duration::from_millis(50));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}

#[test]
fn every_phase_round_trips_through_the_word() {
    let phases = [
        (CrumbPhase::Idle, 0u32),
        (CrumbPhase::Spawning, 1),
        (CrumbPhase::Started, 2),
        (CrumbPhase::StopSignalled, 3),
        (CrumbPhase::Stopped, 4),
        (CrumbPhase::Faulted, 5),
        (CrumbPhase::Resumed, 6),
    ];
    for (phase, v) in phases {
        for slot in [0u16, 7, u16::MAX] {
            assert_eq!(
                decode_crumb(0x8000_0000 | (v << 16) | u32::from(slot)),
                Some((phase, slot)),
                "{} at slot {slot}",
                phase.as_str()
            );
        }
    }
    assert_eq!(
        decode_crumb(0x8100_0001),
        None,
        "bits 24..31 must be clear: not a crumb"
    );
}
