//! A pool over a `divisible` resource: every member holds its own slot, and a
//! member's stop releases that slot alone.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{
    Claimant, DeferredShrink, FairShare, Supervisor, TaskNode, supervisor_graph,
};
use embassy_time::Instant;
use support::settle;

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

fn grants() -> [u32; 4] {
    core::array::from_fn(|i| BAND.grant(i as u8))
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    BAND.provide(100);
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    settle("every member saw its slot", || {
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

    assert!(BAND.rebalance(&FairShare, Instant::now()).is_none());
    assert_eq!(grants(), [40, 20, 20, 20]);

    sup.stop_node(&LINKS[1]).await.expect("acks");
    assert_eq!(
        BAND.want_of(2),
        0,
        "the stopped member's slot, and only that one"
    );
    assert!(BAND.rebalance(&FairShare, Instant::now()).is_none());
    assert_eq!(grants(), [40, 20, 0, 20]);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn pool_members_hold_their_own_budget_slots() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || format!("timed out: grants={:?}", grants()),
    );
}
