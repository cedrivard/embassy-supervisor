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
        Self { cooldown, pending: Mutex::new(Cell::new(None)) }
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
        PoolStats { running, busy, min: self.min, max: self.max }
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
