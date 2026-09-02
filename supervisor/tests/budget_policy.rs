//! The budget primitive and its two policies, driven by hand: no graph, no
//! executor, a noop waker where a future is involved.

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use embassy_supervisor::{Budget, BudgetPolicy, FairShare, ResourceGate, ShrinkFastGrowSlow};
use embassy_time::{Duration, Instant};

fn t0() -> Instant {
    Instant::from_ticks(0)
}

fn fair(capacity: u32, wants: &[u32]) -> Vec<u32> {
    let mut grants = vec![0; wants.len()];
    assert!(
        FairShare
            .divide(capacity, wants, &mut grants, t0())
            .is_none()
    );
    grants
}

#[test]
fn fair_share_grants_wants_in_full_when_they_fit() {
    assert_eq!(fair(100, &[30, 20]), [30, 20]);
    assert_eq!(fair(100, &[0, 50]), [0, 50]);
    assert_eq!(fair(0, &[10, 10]), [0, 0], "no capacity, no grants");
}

#[test]
fn fair_share_splits_in_proportion_and_hands_the_remainder_down() {
    assert_eq!(fair(100, &[60, 60, 0]), [50, 50, 0]);
    assert_eq!(fair(10, &[7, 7]), [5, 5]);
    // 10 * 7 / 11 = 6, 10 * 4 / 11 = 3, one unit left for the lowest slot
    // that can still use it.
    assert_eq!(fair(10, &[7, 4]), [7, 3]);
    assert_eq!(fair(1, &[5, 5, 5]), [1, 0, 0]);
}

#[test]
fn shrink_fast_grow_slow_cuts_at_once_and_ramps_up() {
    let p = ShrinkFastGrowSlow::new(10, Duration::from_secs(1));
    let mut grants = [0u32, 0];
    let next = p.divide(100, &[60, 60], &mut grants, t0());
    assert_eq!(grants, [10, 10], "one step toward 50/50");
    assert_eq!(next, Some(t0() + Duration::from_secs(1)));

    let next = p.divide(
        100,
        &[60, 60],
        &mut grants,
        t0() + Duration::from_millis(500),
    );
    assert_eq!(grants, [10, 10], "not due: nothing moves");
    assert_eq!(
        next,
        Some(t0() + Duration::from_secs(1)),
        "and the deadline stands"
    );

    let next = p.divide(100, &[60, 60], &mut grants, t0() + Duration::from_secs(1));
    assert_eq!(grants, [20, 20]);
    assert_eq!(next, Some(t0() + Duration::from_secs(2)));

    // A cut lands immediately, whatever the ramp deadline says.
    let next = p.divide(
        20,
        &[60, 60],
        &mut grants,
        t0() + Duration::from_millis(1100),
    );
    assert_eq!(grants, [10, 10]);
    assert_eq!(next, None, "settled: nothing is below its target");

    // One holder leaves: its grant is cut to zero now, the other ramps.
    let at = t0() + Duration::from_secs(5);
    let next = p.divide(100, &[60, 0], &mut grants, at);
    assert_eq!(grants, [20, 0]);
    assert_eq!(next, Some(at + Duration::from_secs(1)));
}

fn poll_once<F: Future>(fut: &mut core::pin::Pin<&mut F>) -> Poll<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    fut.as_mut().poll(&mut cx)
}

static B: Budget<3> = Budget::new();

#[test]
fn a_budget_divides_releases_and_clears() {
    assert!(!B.is_filled(), "unprovided reads empty");
    B.provide(100);
    assert!(B.is_filled());
    assert_eq!(B.capacity(), 100);

    let one = B.claimant(0);
    let two = B.claimant(1);
    assert_eq!((one.slot(), two.slot()), (0, 1));
    one.want(60);
    two.want(60);
    assert_eq!(B.want_of(0), 60);
    assert_eq!(one.grant(), 0, "nothing until a rebalance");

    let mut waiting = pin!(two.wait_grant_change(0));
    assert!(poll_once(&mut waiting).is_pending());
    assert!(B.rebalance(&FairShare, t0()).is_none());
    assert_eq!((one.grant(), two.grant()), (50, 50));
    assert_eq!(B.total_granted(), 100);
    assert_eq!(
        poll_once(&mut waiting),
        Poll::Ready(50),
        "the holder is woken with its new grant"
    );

    one.release();
    assert_eq!(
        (B.want_of(0), B.grant(0)),
        (0, 0),
        "released: want and grant both gone"
    );
    B.rebalance(&FairShare, t0());
    assert_eq!(two.grant(), 60, "the survivor gets its whole want");

    ResourceGate::clear(&B);
    assert!(!B.is_filled());
    assert_eq!((B.capacity(), B.grant(1), B.total_granted()), (0, 0, 0));
}

#[test]
fn shrink_fast_grow_slow_divides_over_every_slot() {
    // 33 slots: one past the width of a `u32` bit set, so a policy that kept
    // a fixed scratch array would silently stop at 32.
    let p = ShrinkFastGrowSlow::new(10, Duration::from_secs(1));
    let wants = [2u32; 33];
    let mut grants = [0u32; 33];
    let next = p.divide(40, &wants, &mut grants, t0());
    assert_eq!(grants[32], 1, "the last slot is divided too");
    assert_eq!(
        grants[..7],
        [2; 7],
        "the remainder goes to the lowest slots"
    );
    assert_eq!(grants[7..].iter().sum::<u32>(), 26);
    assert_eq!(grants.iter().sum::<u32>(), 40, "never over the capacity");
    assert_eq!(next, None, "one step of 10 covers every target");

    let next = p.divide(66, &wants, &mut grants, t0() + Duration::from_secs(1));
    assert_eq!(grants, [2; 33], "every want fits: granted in full");
    assert_eq!(next, None);
}

static C: Budget<2> = Budget::new();

/// A policy standing in for a holder on a higher-priority executor: it
/// releases slot 1 between the allocator's snapshot and its publish.
struct ReleasesMidDivision;

impl BudgetPolicy for ReleasesMidDivision {
    fn divide(
        &self,
        capacity: u32,
        wants: &[u32],
        grants: &mut [u32],
        now: Instant,
    ) -> Option<Instant> {
        C.release(1);
        FairShare.divide(capacity, wants, grants, now)
    }
}

#[test]
fn a_change_landing_mid_rebalance_re_arms_the_allocator() {
    C.provide(100);
    C.claimant(0).want(50);
    C.claimant(1).want(50);
    let mut changed = pin!(C.wait_change());
    assert!(poll_once(&mut changed).is_ready(), "the wants armed it");
    let mut changed = pin!(C.wait_change());
    assert!(poll_once(&mut changed).is_pending(), "drained");

    assert!(C.rebalance(&ReleasesMidDivision, t0()).is_none());
    assert_eq!(
        C.grant(1),
        50,
        "published from the snapshot: the released slot's grant came back"
    );
    assert!(
        poll_once(&mut changed).is_ready(),
        "but the allocator is re-armed for the change it published over"
    );
    C.rebalance(&FairShare, t0());
    assert_eq!(
        (C.grant(0), C.grant(1)),
        (50, 0),
        "the next pass corrects it"
    );
    let mut changed = pin!(C.wait_change());
    assert!(poll_once(&mut changed).is_pending(), "and settles");
}
