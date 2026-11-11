mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Observable, Stamped, Supervisor, TaskNode, dataflow, supervisor_graph};
use embassy_supervisor_observe::Counted;
use embassy_time::{Duration, MockDriver};

pub static S: Stamped<Counted<AtomicU32>> = Stamped::new(Counted::new(AtomicU32::new(0)));
static GO: AtomicBool = AtomicBool::new(false);
static PUT_DONE: AtomicBool = AtomicBool::new(false);

/// One `Stamped` unit in ticks: the largest power of two not above a
/// millisecond's worth of ticks (one tick below 1 kHz).
const UNIT: u64 = {
    let per_ms = embassy_time::TICK_HZ / 1000;
    if per_ms > 1 { 1 << per_ms.ilog2() } else { 1 }
};

supervisor_graph! {
    node W = Terminate, deps: [], task: writer, discover;
}

#[dataflow]
async fn writer(node: &'static TaskNode) {
    while !GO.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    node.put(&crate::S, 7);
    PUT_DONE.store(true, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    core::future::pending::<()>().await;
}

#[test]
fn a_stamped_signal_knows_how_old_its_value_is() {
    let clock = MockDriver::get();
    assert_eq!(S.age(), None, "never written");
    assert!(!S.is_fresh(Duration::from_secs(1)));
    assert!(S.read_fresh(Duration::from_secs(1)).is_none());

    S.w().w().store(3, Ordering::SeqCst);
    assert_eq!(S.age(), Some(Duration::from_ticks(0)));
    assert!(S.is_fresh(Duration::from_ticks(0)));
    clock.advance(Duration::from_ticks(200 * UNIT));
    assert_eq!(S.age(), Some(Duration::from_ticks(200 * UNIT)));
    assert!(
        !S.is_fresh(Duration::from_millis(50)),
        "older than the bound"
    );
    assert!(S.is_fresh(Duration::from_millis(200)));
    assert_eq!(
        S.read_fresh(Duration::from_millis(200))
            .map(|c| c.r().load(Ordering::SeqCst)),
        Some(3)
    );
    let token = S.change_token();
    assert_eq!(token, 1, "forwarded: one counted write");
    clock.advance(Duration::from_secs(5));
    assert_eq!(S.change_token(), token, "time passing is not a write");

    // Ages saturate instead of wrapping: a dead signal never reads fresh again.
    let age = S.age().expect("written").as_ticks();
    clock.advance(Duration::from_ticks((1 << 31) * UNIT));
    assert_eq!(
        S.age(),
        Some(Duration::from_ticks(u64::from(u32::MAX / 2) * UNIT))
    );
    clock.advance(Duration::from_ticks((1 << 31) * UNIT - age));
    assert!(
        !S.is_fresh(Duration::from_secs(1)),
        "2^32 units after the write, still stale"
    );
    assert!(
        !S.is_fresh(Duration::from_ticks((1 << 32) * UNIT)),
        "a bound past the cap is clamped"
    );

    // Through the node's verb: `put` stamps after its write.
    GO.store(true, Ordering::SeqCst);
    support::run(
        |spawner| spawner.spawn(runner(spawner).unwrap()),
        &PUT_DONE,
        support::Clock::Frozen,
        10,
        || "the writer never ran".to_owned(),
    );
    assert_eq!(S.age(), Some(Duration::from_ticks(0)), "stamped by the put");
    assert_eq!(S.r().r().load(Ordering::SeqCst), 7);
    assert_eq!(S.change_token(), 2);
}
