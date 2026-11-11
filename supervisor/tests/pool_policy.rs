mod support;

use std::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{
    DeferredShrink, Pool, PoolAction, PoolStats, ScaleAction, ScalingPolicy, Supervisor, TaskNode,
    supervisor_graph,
};
use embassy_time::{Duration, Instant};
use support::settle;

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
    assert_eq!(p.decide(stats(2, 2, 1, 4), t0()), ScaleAction::Grow);
}

#[test]
fn does_not_grow_at_ceiling() {
    let p = DeferredShrink::new(Duration::from_secs(4));
    assert_eq!(p.decide(stats(4, 4, 1, 4), t0()), ScaleAction::None);
}

#[test]
fn defers_then_shrinks_after_cooldown() {
    let cooldown = Duration::from_secs(4);
    let p = DeferredShrink::new(cooldown);
    let now = t0();

    assert_eq!(p.decide(stats(3, 1, 1, 4), now), ScaleAction::None);
    assert_eq!(p.deferred_until(), Some(now + cooldown));

    assert_eq!(
        p.decide(stats(3, 1, 1, 4), now + Duration::from_secs(2)),
        ScaleAction::None
    );

    assert_eq!(
        p.decide(stats(3, 1, 1, 4), now + cooldown),
        ScaleAction::Shrink
    );
}

#[test]
fn cancels_pending_shrink_when_surplus_disappears() {
    let cooldown = Duration::from_secs(4);
    let p = DeferredShrink::new(cooldown);
    let now = t0();

    assert_eq!(p.decide(stats(3, 1, 1, 4), now), ScaleAction::None);
    assert!(p.deferred_until().is_some());

    assert_eq!(
        p.decide(stats(2, 1, 1, 4), now + Duration::from_secs(1)),
        ScaleAction::None
    );
    assert_eq!(p.deferred_until(), None);
}

#[test]
fn grow_clears_pending_shrink() {
    let cooldown = Duration::from_secs(4);
    let p = DeferredShrink::new(cooldown);
    let now = t0();

    assert_eq!(p.decide(stats(3, 1, 1, 4), now), ScaleAction::None);
    assert!(p.deferred_until().is_some());

    assert_eq!(
        p.decide(stats(3, 3, 1, 4), now + Duration::from_secs(1)),
        ScaleAction::Grow
    );
    assert_eq!(p.deferred_until(), None);
}

/// Every small pool shape against a fresh policy: it grows on saturation or
/// below the floor (never past `max`), its first call never shrinks (that call
/// only arms the cooldown), and nothing at or below the floor shrinks even
/// once the cooldown has run out.
#[test]
fn decides_every_small_shape() {
    for running in 0..=8u8 {
        for busy in 0..=running {
            for max in 0..=8u8 {
                for min in 0..=max {
                    let s = stats(running, busy, min, max);
                    let shape = format!("running {running}, busy {busy}, min {min}, max {max}");
                    let p = DeferredShrink::new(Duration::from_secs(1));
                    let first = p.decide(s, t0());
                    let grow = (s.idle() == 0 || running < min) && running < max;
                    assert_eq!(
                        first == ScaleAction::Grow,
                        grow,
                        "{shape}: Grow iff saturated or below min, and below max"
                    );
                    assert_ne!(
                        first,
                        ScaleAction::Shrink,
                        "{shape}: the first call only arms the cooldown"
                    );
                    if running <= min {
                        let later = p.decide(s, t0() + Duration::from_secs(1));
                        assert_ne!(
                            later,
                            ScaleAction::Shrink,
                            "{shape}: never shrinks at or below the floor"
                        );
                    }
                }
            }
        }
    }
}

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

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    let now = t0();

    // Floor up and busy → saturated, below max → evaluate picks the first
    // down OnDemand member.
    sup.start(&spawner).await.expect("start floor");
    settle("floor member running", || CREW[0].is_running()).await;
    CREW[0].mark_busy();
    match CREW_POOL.evaluate(now) {
        PoolAction::Start(n) => assert!(core::ptr::eq(n, &CREW[1]), "first down member"),
        other => panic!("expected Start, got {other:?}"),
    }

    // Disabled members are never grow candidates: with both spares disabled the
    // saturated pool has nowhere to go.
    CREW[1].set_disabled(true);
    CREW[2].set_disabled(true);
    let action = CREW_POOL.evaluate(now);
    assert!(matches!(action, PoolAction::None), "{action:?}");
    CREW[1].set_disabled(false);
    CREW[2].set_disabled(false);

    // Nor are detached ones: `start_node` leaves a detached node alone, so
    // picking one would stall growth on it.
    CREW[1].set_detached(true);
    match CREW_POOL.evaluate(now) {
        PoolAction::Start(n) => assert!(core::ptr::eq(n, &CREW[2]), "the next down member"),
        other => panic!("expected Start, got {other:?}"),
    }
    CREW[1].set_detached(false);

    // All three up, none busy → idle surplus: first evaluate arms the
    // cooldown, the one at +cooldown stops an idle OnDemand member: never the
    // floor, and never a detached one, which `stop_node` would leave running.
    sup.start_node(&CREW[1], &spawner).await.expect("grow 1");
    sup.start_node(&CREW[2], &spawner).await.expect("grow 2");
    settle("both spares running", || {
        CREW[1].is_running() && CREW[2].is_running()
    })
    .await;
    CREW[0].mark_idle();
    CREW[1].set_detached(true);
    let action = CREW_POOL.evaluate(now);
    assert!(
        matches!(action, PoolAction::None),
        "first tick arms cooldown, got {action:?}"
    );
    match CREW_POOL.evaluate(now + Duration::from_secs(4)) {
        PoolAction::Stop(n) => assert!(
            core::ptr::eq(n, &CREW[2]),
            "the idle OnDemand member that is not detached"
        ),
        other => panic!("expected Stop, got {other:?}"),
    }
    CREW[1].set_detached(false);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn evaluate_selects_members_through_real_state() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
}
