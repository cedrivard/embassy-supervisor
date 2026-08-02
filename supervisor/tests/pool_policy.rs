//! Pool scaling behavior, all through the public API. The `DeferredShrink`
//! policy tests are pure (decide/deferred_until take `now` explicitly — no time
//! driver fires). The `ElasticPool::evaluate` tests drive real node state
//! (running/busy/disabled) via a macro graph and an executor on a std thread,
//! then interrogate the pool object directly with synthetic instants.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    DeferredShrink, Pool, PoolAction, PoolStats, ScaleAction, ScalingPolicy, Supervisor, TaskNode,
    supervisor_graph,
};
use embassy_time::{Duration, Instant};

// ── DeferredShrink policy (pure) ────────────────────────────────────────────

/// A fixed base instant (tick 0); offset it with `Duration` arithmetic.
fn t0() -> Instant {
    Instant::from_ticks(0)
}

fn stats(running: u8, busy: u8, min: u8, max: u8) -> PoolStats {
    PoolStats {
        running,
        busy,
        min,
        max,
    }
}

#[test]
fn grows_when_saturated_below_max() {
    let p = DeferredShrink::new(Duration::from_secs(4));
    // idle == 0 (all busy), running < max → grow immediately.
    assert!(p.decide(stats(2, 2, 1, 4), t0()) == ScaleAction::Grow);
}

#[test]
fn does_not_grow_at_ceiling() {
    let p = DeferredShrink::new(Duration::from_secs(4));
    assert!(p.decide(stats(4, 4, 1, 4), t0()) == ScaleAction::None);
}

#[test]
fn defers_then_shrinks_after_cooldown() {
    let cooldown = Duration::from_secs(4);
    let p = DeferredShrink::new(cooldown);
    let now = t0();

    // Surplus (idle 2, running > min): first sight arms the cooldown, no action.
    assert!(p.decide(stats(3, 1, 1, 4), now) == ScaleAction::None);
    assert_eq!(p.deferred_until(), Some(now + cooldown));

    // Still inside the window → hold.
    assert!(p.decide(stats(3, 1, 1, 4), now + Duration::from_secs(2)) == ScaleAction::None);

    // Cooldown elapsed → shrink one spare.
    assert!(p.decide(stats(3, 1, 1, 4), now + cooldown) == ScaleAction::Shrink);
}

#[test]
fn cancels_pending_shrink_when_surplus_disappears() {
    let cooldown = Duration::from_secs(4);
    let p = DeferredShrink::new(cooldown);
    let now = t0();

    assert!(p.decide(stats(3, 1, 1, 4), now) == ScaleAction::None); // arm
    assert!(p.deferred_until().is_some());

    // idle drops to 1 (not saturated, not surplus) → pending cleared.
    assert!(p.decide(stats(2, 1, 1, 4), now + Duration::from_secs(1)) == ScaleAction::None);
    assert_eq!(p.deferred_until(), None);
}

#[test]
fn grow_clears_pending_shrink() {
    let cooldown = Duration::from_secs(4);
    let p = DeferredShrink::new(cooldown);
    let now = t0();

    assert!(p.decide(stats(3, 1, 1, 4), now) == ScaleAction::None); // arm
    assert!(p.deferred_until().is_some());

    // Becomes saturated → grow and cancel the pending shrink.
    assert!(p.decide(stats(3, 3, 1, 4), now + Duration::from_secs(1)) == ScaleAction::Grow);
    assert_eq!(p.deferred_until(), None);
}

// ── ElasticPool::evaluate (real node state, public API only) ────────────────

supervisor_graph! {
    pool CREW = [Terminate, OnDemand, OnDemand], deps: [],
        task: crew_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: 1, max: 3;
}

async fn crew_worker(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

static DONE: AtomicBool = AtomicBool::new(false);

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
    let now = t0();

    // Floor up and busy → saturated, below max → evaluate picks the first
    // down OnDemand member.
    sup.start(spawner).await.expect("start floor");
    settle(|| CREW[0].is_running()).await;
    CREW[0].mark_busy();
    match CREW_POOL.evaluate(now) {
        PoolAction::Start(n) => assert!(core::ptr::eq(n, &CREW[1]), "first down member"),
        _ => panic!("expected Start"),
    }

    // Disabled members are never grow candidates: with both spares disabled the
    // saturated pool has nowhere to go.
    CREW[1].set_disabled(true);
    CREW[2].set_disabled(true);
    assert!(matches!(CREW_POOL.evaluate(now), PoolAction::None));
    CREW[1].set_disabled(false);
    CREW[2].set_disabled(false);

    // All three up, none busy → idle surplus: first evaluate arms the
    // cooldown, the one at +cooldown stops an idle OnDemand member (never the
    // floor).
    sup.start_node(&CREW[1], spawner).await.expect("grow 1");
    sup.start_node(&CREW[2], spawner).await.expect("grow 2");
    settle(|| CREW[1].is_running() && CREW[2].is_running()).await;
    CREW[0].mark_idle();
    assert!(
        matches!(CREW_POOL.evaluate(now), PoolAction::None),
        "first tick arms cooldown"
    );
    match CREW_POOL.evaluate(now + Duration::from_secs(4)) {
        PoolAction::Stop(n) => {
            assert!(
                core::ptr::eq(n, &CREW[1]) || core::ptr::eq(n, &CREW[2]),
                "an idle OnDemand member, never the floor"
            );
        }
        _ => panic!("expected Stop"),
    }

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn evaluate_selects_members_through_real_state() {
    let _clock = embassy_time::MockDriver::get();

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
