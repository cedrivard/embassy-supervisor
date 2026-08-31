use super::*;

use core::cell::Cell;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Duration, Instant};

#[derive(Clone, Copy)]
/// Snapshot of an elastic pool's current load.
pub struct PoolStats {
    /// Number of currently running members.
    pub running: u8,
    /// Number of running members marked busy.
    pub busy: u8,
    /// Minimum member count.
    pub min: u8,
    /// Maximum member count.
    pub max: u8,
}
impl PoolStats {
    /// Number of running members that are not busy.
    pub fn idle(&self) -> u8 {
        self.running.saturating_sub(self.busy)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
/// Decision an elastic scaling policy can return.
pub enum ScaleAction {
    /// No change.
    None,
    /// Start another member.
    Grow,
    /// Stop an idle member.
    Shrink,
}

/// Policy that decides when an elastic pool should grow or shrink.
pub trait ScalingPolicy {
    /// Decide the next scaling action given current pool statistics.
    fn decide(&self, stats: PoolStats, now: Instant) -> ScaleAction;

    /// Optional future instant at which the policy should be re-evaluated.
    fn deferred_until(&self) -> Option<Instant> {
        None
    }
}

/// A scaling policy that grows immediately but shrinks only after a cooldown.
pub struct DeferredShrink {
    cooldown: Duration,
    pending: Mutex<CriticalSectionRawMutex, Cell<Option<Instant>>>,
}
impl DeferredShrink {
    /// Create a policy with the given shrink cooldown.
    pub const fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            pending: Mutex::new(Cell::new(None)),
        }
    }
}
impl ScalingPolicy for DeferredShrink {
    fn decide(&self, s: PoolStats, now: Instant) -> ScaleAction {
        if s.idle() == 0 && s.running < s.max {
            self.pending.lock(|p| p.set(None));
            return ScaleAction::Grow;
        }
        if s.idle() >= 2 && s.running > s.min {
            match self.pending.lock(|p| p.get()) {
                None => {
                    self.pending.lock(|p| p.set(Some(now + self.cooldown)));
                    ScaleAction::None
                }
                Some(deadline) if now >= deadline => {
                    let next = (s.idle() >= 3).then(|| now + self.cooldown);
                    self.pending.lock(|p| p.set(next));
                    ScaleAction::Shrink
                }
                Some(_) => ScaleAction::None,
            }
        } else {
            self.pending.lock(|p| p.set(None));
            ScaleAction::None
        }
    }

    fn deferred_until(&self) -> Option<Instant> {
        self.pending.lock(|p| p.get())
    }
}

/// Action the supervisor should take for a pool member.
pub enum PoolAction {
    /// No action.
    None,
    /// Start this member.
    Start(&'static TaskNode),
    /// Stop this (running, idle) pool member.
    Stop(&'static TaskNode),
}

/// An elastic pool backed by a [`ScalingPolicy`].
pub struct ElasticPool<P: ScalingPolicy> {
    /// Pool member nodes.
    pub nodes: &'static [&'static TaskNode],
    /// Minimum number of members.
    pub min: u8,
    /// Maximum number of members.
    pub max: u8,
    /// Scaling policy instance.
    pub policy: P,
}

impl<P: ScalingPolicy> ElasticPool<P> {
    /// Return the pool index of `node`, if it belongs to this pool.
    pub fn member_index(&self, node: &'static TaskNode) -> Option<usize> {
        self.nodes.iter().position(|m| core::ptr::eq(*m, node))
    }

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
/// Object-safe, synchronous pool interface used by the supervisor.
pub trait Pool: Sync {
    /// Run the policy against the current snapshot and report the action to
    /// apply. Does not itself start/stop (that's async — the caller does it).
    fn evaluate(&self, now: Instant) -> PoolAction;
    /// Optional future instant at which the pool should be re-evaluated.
    fn deferred_until(&self) -> Option<Instant>;
    /// Return the pool's member nodes.
    fn members(&self) -> &'static [&'static TaskNode];
}

impl<P: ScalingPolicy + Sync> Pool for ElasticPool<P> {
    fn evaluate(&self, now: Instant) -> PoolAction {
        match self.policy.decide(self.stats(), now) {
            ScaleAction::Grow => self
                .nodes
                .iter()
                .find(|n| {
                    matches!(n.mode(), Mode::OnDemand)
                        && !n.is_running()
                        && !n.is_disabled()
                        && !n.is_collateral()
                })
                .map_or(PoolAction::None, |n| PoolAction::Start(n)),
            ScaleAction::Shrink => self
                .nodes
                .iter()
                .find(|n| matches!(n.mode(), Mode::OnDemand) && n.is_running() && !n.is_busy())
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

async fn drive_pools<const N: usize, T: Topology<N>>(
    pools: &[&dyn Pool],
    sup: &Supervisor<N, T>,
    spawner: &Spawner,
) -> Result<Option<Instant>, crate::NodeFault> {
    let now = Instant::now();
    let mut next: Option<Instant> = None;
    for pool in pools {
        match pool.evaluate(now) {
            PoolAction::Start(n) => {
                if sup.deps_running(n) && n.ready_deps_ok() {
                    let _ = sup.start_node(n, spawner).await;
                }
            }
            PoolAction::Stop(n) => sup.stop_node(n).await?,
            PoolAction::None => {}
        }
        if let Some(d) = pool.deferred_until() {
            next = Some(next.map_or(d, |c| c.min(d)));
        }
    }
    Ok(next)
}

async fn deadline_timer(deadline: Option<Instant>) {
    match deadline {
        Some(t) => Timer::at(t).await,
        None => core::future::pending::<()>().await,
    }
}

impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    /// Drive all elastic pools forever, returning only on a fault.
    pub async fn run_pools(&self, spawner: &Spawner) -> crate::NodeFault {
        if !Self::has(crate::shape::POOLS) {
            let never: core::convert::Infallible = core::future::pending().await;
            match never {}
        }
        loop {
            let next = match drive_pools(self.pools, self, spawner).await {
                Ok(next) => next,
                Err(e) => return e,
            };
            select(wait_scale(), deadline_timer(next)).await;
        }
    }
}
