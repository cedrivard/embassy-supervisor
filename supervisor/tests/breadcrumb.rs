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

static DONE: AtomicBool = AtomicBool::new(false);

async fn plain(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn gated(node: &'static TaskNode, _slot: &mut u32) {
    node.wait_shutdown().await;
    node.mark_exited();
}

fn slot(node: &'static TaskNode) -> u16 {
    u16::from(GRAPH.index_of(node).expect("node is in GRAPH"))
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

    while !DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "did not complete");
        clock.advance(embassy_time::Duration::from_millis(50));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
