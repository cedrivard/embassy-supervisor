mod support;

use std::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{CrumbPhase, Supervisor, TaskNode, decode_crumb, supervisor_graph};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node FREE = Terminate, deps: [], task: worker;
    node FIRST = Terminate, deps: [], task: worker, beat_timeout: 100;
    node SECOND = Terminate, deps: [], task: worker, beat_timeout: 100;
}

static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn settle() {
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

fn blamed() -> Option<(CrumbPhase, u16)> {
    GRAPH.stale_crumb().and_then(decode_crumb)
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    let sup = Supervisor::new(&GRAPH);

    sup.start(&spawner).await.expect("start");
    settle().await;
    assert_eq!(GRAPH.stale_crumb(), None, "every policed node is fresh");

    clock.advance(Duration::from_millis(150));
    assert_eq!(
        blamed(),
        Some((CrumbPhase::Stale, 1)),
        "both are stale: the first policed slot is blamed, never the unpoliced FREE"
    );

    FIRST.beat();
    assert_eq!(
        blamed(),
        Some((CrumbPhase::Stale, 2)),
        "a beat clears FIRST, so the word names SECOND's slot"
    );

    let last = decode_crumb(embassy_supervisor::crumb_word()).expect("a transition ran");
    assert_ne!(
        last.0,
        CrumbPhase::Stale,
        "the supervisor never records Stale"
    );
    assert_ne!(SECOND.crumb(), CrumbPhase::Stale);

    sup.stop_node(&SECOND).await.expect("stop");
    assert_eq!(GRAPH.stale_crumb(), None, "a stopped node is never blamed");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn stale_crumb_blames_the_first_stale_policed_slot() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
}
