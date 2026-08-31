//! `deactivate`/`activate` symmetry over a subtree: the seed gets the
//! `disabled` latch, dependents get the `collateral` hold, and `activate` on
//! the seed releases exactly the dependents with no disabled node left among
//! their transitive deps — so overlapping deactivations compose, a direct
//! deactivation survives its ancestor's cycle, and pool members become grow
//! candidates again instead of staying latched forever.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    DeferredShrink, Pool, PoolAction, Supervisor, TaskNode, supervisor_graph,
};
use embassy_time::Instant;

static DONE: AtomicBool = AtomicBool::new(false);

async fn hold_worker(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node ROOT = Terminate, deps: [], task: hold_worker;
    node LINK = Terminate, deps: [], task: hold_worker;
    node MID = Terminate, deps: [LINK], task: hold_worker;
    node DIAMOND = Terminate, deps: [LINK, ROOT], task: hold_worker;
    pool CREW = [OnDemand, OnDemand], deps: [LINK],
        task: hold_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: 0, max: 2;
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    let now = Instant::from_ticks(0);

    sup.start(&spawner).await.expect("bring-up");
    settle(|| ROOT.is_running() && LINK.is_running() && MID.is_running() && DIAMOND.is_running())
        .await;

    // Grow one member by hand so the deactivation has a running member to stop.
    sup.start_node(&CREW[0], &spawner).await.expect("grow");
    settle(|| CREW[0].is_running()).await;

    // Deactivate LINK: the seed alone is disabled; dependents are held
    // collateral — stopped, but never deactivated in their own right.
    sup.deactivate(&LINK).await.expect("deactivate LINK");
    assert!(LINK.is_disabled() && !LINK.is_collateral() && !LINK.is_running());
    for n in [&MID, &DIAMOND, &CREW[0], &CREW[1]] {
        assert!(!n.is_disabled(), "{} is not disabled", n.name());
        assert!(n.is_collateral(), "{} is held collateral", n.name());
        assert!(!n.is_running(), "{} is stopped", n.name());
    }
    assert!(
        matches!(CREW_POOL.evaluate(now), PoolAction::None),
        "held members are not grow candidates"
    );

    // Overlap the diamond with a second deactivation.
    sup.deactivate(&ROOT).await.expect("deactivate ROOT");
    assert!(ROOT.is_disabled() && !DIAMOND.is_disabled());

    // Activate LINK: MID revives, the members become grow candidates again
    // (released, not started — demand regrows OnDemand), and DIAMOND stays
    // held because ROOT is still disabled.
    sup.activate(&LINK, &spawner).await;
    settle(|| LINK.is_running() && MID.is_running()).await;
    assert!(MID.is_running() && !MID.is_collateral());
    assert!(!CREW[0].is_collateral() && !CREW[1].is_collateral());
    assert!(
        !CREW[0].is_running() && !CREW[1].is_running(),
        "OnDemand members are released, not started"
    );
    assert!(matches!(CREW_POOL.evaluate(now), PoolAction::Start(_)));
    assert!(
        DIAMOND.is_collateral() && !DIAMOND.is_running(),
        "still held: ROOT remains disabled"
    );

    // Activate ROOT: the last disabled ancestor is gone, DIAMOND revives.
    sup.activate(&ROOT, &spawner).await;
    settle(|| DIAMOND.is_running()).await;
    assert!(DIAMOND.is_running() && !DIAMOND.is_collateral());

    // A node deactivated in its own right survives its ancestor's cycle.
    sup.deactivate(&MID).await.expect("deactivate MID");
    sup.deactivate(&LINK).await.expect("deactivate LINK again");
    sup.activate(&LINK, &spawner).await;
    settle(|| LINK.is_running()).await;
    assert!(
        MID.is_disabled() && !MID.is_running(),
        "a direct deactivation is not undone by the ancestor's activate"
    );
    sup.activate(&MID, &spawner).await;
    settle(|| MID.is_running()).await;
    assert!(MID.is_running());

    // A member-targeted deactivate latches the whole pool disabled; only its
    // own activate releases it.
    sup.deactivate(&CREW[0]).await.expect("deactivate member");
    assert!(CREW[0].is_disabled() && CREW[1].is_disabled());
    assert!(matches!(CREW_POOL.evaluate(now), PoolAction::None));
    sup.activate(&CREW[0], &spawner).await;
    assert!(!CREW[0].is_disabled() && !CREW[1].is_disabled());
    assert!(matches!(CREW_POOL.evaluate(now), PoolAction::Start(_)));

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn collateral_hold_released_by_activate() {
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
            "did not complete (link={}, mid={}, diamond={}, member0={})",
            LINK.is_running(),
            MID.is_running(),
            DIAMOND.is_running(),
            CREW[0].is_running(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
