//! Reusable elastic-pool scaling.
//!
//! A pool is a floor node (always-on `Terminate`) plus on-demand workers of the
//! same task fn, scaled by a swappable [`ScalingPolicy`]. The policy is a generic
//! type parameter (static dispatch, zero-cost), and may be stateful via interior
//! mutability since the pool lives in a `static`.
//!
//! Heterogeneous pools are driven uniformly through the object-safe [`Pool`]
//! trait. Its methods are **synchronous** — the policy only *decides* (returns a
//! [`PoolAction`]); the supervisor performs the actual async `start_node` /
//! `stop_node`. That keeps `&dyn Pool` object-safe with **no boxed futures and no
//! heap**, while policies stay generic and zero-cost.

use super::*;

use core::cell::Cell;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Duration, Instant};

/// Aggregate state of a pool, handed to the policy.
#[derive(Clone, Copy)]
pub struct PoolStats {
    /// Instances currently up (spawned and not exited).
    pub running: u8,
    /// Of the running instances, how many are serving (marked busy).
    pub busy: u8,
    /// Floor — the pool never shrinks below this.
    pub min: u8,
    /// Ceiling — the pool never grows above this.
    pub max: u8,
}
impl PoolStats {
    /// Instances that are up but not serving (the spares).
    pub fn idle(&self) -> u8 {
        self.running.saturating_sub(self.busy)
    }
}

/// What a policy wants done this evaluation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScaleAction {
    /// Leave the pool at its current size.
    None,
    /// Start one more instance (if below `max`).
    Grow,
    /// Stop one idle instance (if above `min`).
    Shrink,
}

/// Swappable scaling decision. `decide` is synchronous; stateful policies use
/// interior mutability (the pool is a `static`, so `&self`).
pub trait ScalingPolicy {
    /// Decide what to do given the current pool `stats` at time `now`.
    fn decide(&self, stats: PoolStats, now: Instant) -> ScaleAction;

    /// The next instant at which the pool must be re-evaluated even without a
    /// status signal (e.g. a deferred shrink's cooldown). `None` = nothing
    /// pending. The supervisor arms a one-shot timer for it.
    fn deferred_until(&self) -> Option<Instant> {
        None
    }
}

/// Grow immediately (stay responsive), but shrink only after the idle surplus has
/// persisted for `cooldown` — damps grow→shrink→grow flapping. Holds its pending
/// shrink **deadline** in a `Cell<Option<Instant>>` (interior mutability under
/// `&self`, since the pool is a `static`); `None` = no shrink pending.
pub struct DeferredShrink {
    cooldown: Duration,
    /// Pending shrink deadline, or `None`. A critical-section mutex over a `Cell`
    /// (not an `AtomicU64`) so it's Sync + const-constructible without pulling in
    /// `portable_atomic`'s 64-bit lock-table fallback on Cortex-M.
    pending: Mutex<CriticalSectionRawMutex, Cell<Option<Instant>>>,
}
impl DeferredShrink {
    /// Create a policy that defers each shrink by `cooldown` after the pool first
    /// becomes over-provisioned.
    pub const fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            pending: Mutex::new(Cell::new(None)),
        }
    }
}
impl ScalingPolicy for DeferredShrink {
    fn decide(&self, s: PoolStats, now: Instant) -> ScaleAction {
        // Grow immediately, cancelling any pending shrink (we need this one).
        if s.idle() == 0 && s.running < s.max {
            self.pending.lock(|p| p.set(None));
            return ScaleAction::Grow;
        }
        // Shrink only after the surplus has persisted the whole cooldown.
        if s.idle() >= 2 && s.running > s.min {
            match self.pending.lock(|p| p.get()) {
                None => {
                    // First sight of surplus — arm the cooldown.
                    self.pending.lock(|p| p.set(Some(now + self.cooldown)));
                    ScaleAction::None
                }
                Some(deadline) if now >= deadline => {
                    // Surplus held for the full cooldown — shrink one spare.
                    // Re-arm only if a surplus will remain afterwards (idle-1 >=
                    // 2), else clear (avoids a trailing no-op wake).
                    let next = (s.idle() >= 3).then(|| now + self.cooldown);
                    self.pending.lock(|p| p.set(next));
                    ScaleAction::Shrink
                }
                Some(_) => ScaleAction::None, // still within the window
            }
        } else {
            // No surplus (or at the floor) — cancel any pending shrink.
            self.pending.lock(|p| p.set(None));
            ScaleAction::None
        }
    }

    fn deferred_until(&self) -> Option<Instant> {
        self.pending.lock(|p| p.get())
    }
}

/// What the supervisor should do for a pool this tick. The async part (start /
/// stop) is applied by the caller, keeping `Pool` object-safe without futures.
pub enum PoolAction {
    /// Nothing to do this tick.
    None,
    /// Start this (currently down) pool member.
    Start(&'static TaskNode),
    /// Stop this (running, idle) pool member.
    Stop(&'static TaskNode),
}

/// An elastic pool of single-instance nodes scaled by policy `P`.
pub struct ElasticPool<P: ScalingPolicy> {
    /// The pool's member nodes (each a single-instance `OnDemand`/`Terminate` node).
    pub nodes: &'static [&'static TaskNode],
    /// Floor — keep at least this many members running.
    pub min: u8,
    /// Ceiling — never run more than this many members.
    pub max: u8,
    /// The scaling policy driving grow/shrink decisions.
    pub policy: P,
}

impl<P: ScalingPolicy> ElasticPool<P> {
    fn stats(&self) -> PoolStats {
        // One pass: count running nodes, and the busy subset of those.
        let (running, busy) = self.nodes.iter().fold((0u8, 0u8), |(r, b), n| {
            if n.is_running() {
                (r + 1, b + n.is_busy() as u8)
            } else {
                (r, b)
            }
        });
        PoolStats {
            running,
            busy,
            min: self.min,
            max: self.max,
        }
    }
}

/// Object-safe, **synchronous** pool interface so `&dyn Pool` needs no heap: the
/// policy decides here; the supervisor performs the async start/stop.
pub trait Pool: Sync {
    /// Run the policy against the current snapshot and report the action to
    /// apply. Does not itself start/stop (that's async — the caller does it).
    fn evaluate(&self, now: Instant) -> PoolAction;
    /// Earliest instant this pool must be re-evaluated without a signal.
    fn deferred_until(&self) -> Option<Instant>;
    /// The pool's member nodes (floor first). Used by the supervisor's control
    /// interface to co-control a whole pool from any member, and by a task-state
    /// view to group members. This is the single source of pool membership — the
    /// same slice the scaling policy iterates.
    fn members(&self) -> &'static [&'static TaskNode];
}

impl<P: ScalingPolicy + Sync> Pool for ElasticPool<P> {
    fn evaluate(&self, now: Instant) -> PoolAction {
        match self.policy.decide(self.stats(), now) {
            // Grow only a candidate that is OnDemand, down, **not manually
            // disabled**, and whose deps are all running. The disabled check is
            // what keeps a manually-stopped pool from being re-grown by the
            // policy; the deps check keeps the pool from spawning a worker while
            // one of its dependencies is down (e.g. after a manual stop of a
            // dependency cascaded the pool down).
            ScaleAction::Grow => self
                .nodes
                .iter()
                .find(|n| {
                    matches!(n.mode, Mode::OnDemand)
                        && !n.is_running()
                        && !n.is_disabled()
                        && n.deps.iter().all(|d| d.is_running())
                })
                .map_or(PoolAction::None, |n| PoolAction::Start(n)),
            ScaleAction::Shrink => self
                .nodes
                .iter()
                .find(|n| matches!(n.mode, Mode::OnDemand) && n.is_running() && !n.is_busy())
                .map_or(PoolAction::None, |n| PoolAction::Stop(n)),
            ScaleAction::None => PoolAction::None,
        }
    }

    fn deferred_until(&self) -> Option<Instant> {
        self.policy.deferred_until()
    }

    fn members(&self) -> &'static [&'static TaskNode] {
        self.nodes
    }
}

/// Run every pool's policy and apply its chosen scaling action (evaluate is
/// sync; the async start/stop happens here), returning the earliest deferred
/// re-evaluation deadline across all pools, or `None`.
async fn drive_pools<const N: usize>(
    pools: &[&dyn Pool],
    sup: &Supervisor<N>,
    spawner: Spawner,
) -> Option<Instant> {
    let now = Instant::now();
    let mut next: Option<Instant> = None;
    for pool in pools {
        match pool.evaluate(now) {
            PoolAction::Start(n) => {
                // SpawnError::Busy at the pool ceiling → can't grow, no-op.
                let _ = sup.start_node(n, spawner);
            }
            PoolAction::Stop(n) => sup.stop_node(n).await,
            PoolAction::None => {}
        }
        if let Some(d) = pool.deferred_until() {
            next = Some(next.map_or(d, |c| c.min(d)));
        }
    }
    next
}

/// Future that fires at `deadline` if `Some`, else never — a pool's deferred
/// re-evaluation wake (e.g. a shrink cooldown). Only armed while a deferral is
/// outstanding, so an idle system never polls.
async fn deadline_timer(deadline: Option<Instant>) {
    match deadline {
        Some(t) => Timer::at(t).await,
        None => core::future::pending::<()>().await,
    }
}

impl<const N: usize> Supervisor<N> {
    /// Drive the registered elastic pools (from `with_pools`) forever: run their
    /// policies, then park until the next status signal (`SCALE_REQ`) or a pool's
    /// deferred deadline. Never completes — meant to be `select`ed against the
    /// application's control / teardown futures in the supervisor task. When
    /// another arm wins this future is dropped, which is safe: a half-applied
    /// stop is re-driven on the next pass.
    pub async fn run_pools(&self, spawner: Spawner) {
        loop {
            let next = drive_pools(self.pools, self, spawner).await;
            select(wait_scale(), deadline_timer(next)).await;
        }
    }
}

// ─── Tests (host-only) ─────────────────────────────────────────────────────
//
// Pure scaling-policy logic: `decide` and `evaluate` take `now` as a parameter,
// so no time driver is *called*; `Instant`s are built with `from_ticks` and offset
// by `Duration`s. The policy's state cell is behind a `CriticalSectionRawMutex`, so
// a critical-section impl is needed at link time — provided by the dev-dependency.
// Run with `cargo test -p embassy-supervisor --target x86_64-unknown-linux-gnu`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mode, TaskNode};
    use embassy_executor::{SpawnError, Spawner};
    use embassy_time::{Duration, Instant};

    /// Spawn fn never invoked by these tests (no executor runs).
    fn noop(_: Spawner) -> Result<(), SpawnError> {
        Ok(())
    }

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

    // ── DeferredShrink policy ──────────────────────────────────────────────

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

    // ── ElasticPool::evaluate ──────────────────────────────────────────────

    #[test]
    fn pool_grows_a_down_member_when_saturated() {
        static N0: TaskNode = TaskNode::new("p0", Mode::Terminate, &[], noop);
        static N1: TaskNode = TaskNode::new("p1", Mode::OnDemand, &[], noop);
        static N2: TaskNode = TaskNode::new("p2", Mode::OnDemand, &[], noop);
        static POOL: ElasticPool<DeferredShrink> = ElasticPool {
            nodes: &[&N0, &N1, &N2],
            min: 1,
            max: 3,
            policy: DeferredShrink::new(Duration::from_secs(4)),
        };

        // Only the floor is up and it's busy → saturated, below max → start a spare.
        N0.set_running(true);
        N0.mark_busy();
        match POOL.evaluate(t0()) {
            PoolAction::Start(n) => assert!(core::ptr::eq(n, &N1), "first down OnDemand member"),
            _ => panic!("expected Start"),
        }
    }

    #[test]
    fn pool_shrinks_an_idle_member_after_cooldown() {
        static N0: TaskNode = TaskNode::new("p0", Mode::Terminate, &[], noop);
        static N1: TaskNode = TaskNode::new("p1", Mode::OnDemand, &[], noop);
        static N2: TaskNode = TaskNode::new("p2", Mode::OnDemand, &[], noop);
        static POOL: ElasticPool<DeferredShrink> = ElasticPool {
            nodes: &[&N0, &N1, &N2],
            min: 1,
            max: 3,
            policy: DeferredShrink::new(Duration::from_secs(4)),
        };

        // All three up, none busy → idle surplus.
        N0.set_running(true);
        N1.set_running(true);
        N2.set_running(true);
        let now = t0();

        assert!(
            matches!(POOL.evaluate(now), PoolAction::None),
            "first tick arms cooldown"
        );
        match POOL.evaluate(now + Duration::from_secs(4)) {
            PoolAction::Stop(n) => {
                assert!(
                    core::ptr::eq(n, &N1) || core::ptr::eq(n, &N2),
                    "an idle OnDemand member"
                );
            }
            _ => panic!("expected Stop"),
        }
    }

    #[test]
    fn pool_does_not_grow_a_disabled_member() {
        static N0: TaskNode = TaskNode::new("p0", Mode::Terminate, &[], noop);
        static N1: TaskNode = TaskNode::new("p1", Mode::OnDemand, &[], noop);
        static POOL: ElasticPool<DeferredShrink> = ElasticPool {
            nodes: &[&N0, &N1],
            min: 1,
            max: 2,
            policy: DeferredShrink::new(Duration::from_secs(4)),
        };

        N0.set_running(true);
        N0.mark_busy(); // saturated → policy wants to grow
        N1.set_disabled(true); // but the only candidate is manually disabled
        assert!(matches!(POOL.evaluate(t0()), PoolAction::None));
    }
}
