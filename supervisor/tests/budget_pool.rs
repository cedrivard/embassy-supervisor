//! A pool over a `divisible` resource: every member holds its own slot, and a
//! member's stop releases that slot alone.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    Claimant, DeferredShrink, FairShare, Supervisor, TaskNode, supervisor_graph,
};
use embassy_time::{Duration, Instant, MockDriver};

static SLOTS_SEEN: [AtomicU32; 3] = [const { AtomicU32::new(u32::MAX) }; 3];
static DONE: AtomicBool = AtomicBool::new(false);

supervisor_graph! {
    node HEAD = Terminate, deps: [], task: head, resources: [BAND: divisible];
    pool LINKS = [Terminate, Terminate, Terminate], deps: [], task: link,
        resources: [BAND: divisible],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 3, max: 3;
}

async fn head(node: &'static TaskNode, band: Claimant) {
    band.want(40);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn link(node: &'static TaskNode, band: Claimant) {
    // Members index 0..3 within the pool; their slots follow HEAD's.
    let member = node
        .name()
        .trim_start_matches("links")
        .parse::<usize>()
        .unwrap();
    SLOTS_SEEN[member].store(u32::from(band.slot()), Ordering::SeqCst);
    band.want(20);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..20_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

fn grants() -> [u32; 4] {
    core::array::from_fn(|i| BAND.grant(i as u8))
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    BAND.provide(100);
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    settle(|| {
        SLOTS_SEEN
            .iter()
            .all(|s| s.load(Ordering::SeqCst) != u32::MAX)
    })
    .await;
    let slots: Vec<u32> = SLOTS_SEEN
        .iter()
        .map(|s| s.load(Ordering::SeqCst))
        .collect();
    assert_eq!(
        slots,
        [1, 2, 3],
        "one slot per member, after the node declared first"
    );

    BAND.rebalance(&FairShare, Instant::now());
    assert_eq!(grants(), [40, 20, 20, 20]);

    sup.stop_node(&LINKS[1]).await.expect("acks");
    assert_eq!(
        BAND.want_of(2),
        0,
        "the stopped member's slot, and only that one"
    );
    BAND.rebalance(&FairShare, Instant::now());
    assert_eq!(grants(), [40, 20, 0, 20]);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn pool_members_hold_their_own_budget_slots() {
    let clock = MockDriver::get();
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
            "timed out: grants={:?}",
            grants()
        );
        clock.advance(Duration::from_millis(10));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
