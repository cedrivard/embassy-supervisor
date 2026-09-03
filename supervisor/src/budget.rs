//! A divisible resource: one budget of units, split among the nodes that
//! declare `resources: [NAME: divisible]`, with every holder's share released
//! by the supervisor when that holder stops.

use core::cell::Cell;
use core::task::{Poll, Waker};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_sync::waitqueue::AtomicWaker;
use embassy_time::{Duration, Instant};
use portable_atomic::{AtomicU32, Ordering};

use crate::ResourceGate;

/// A budget of `u32` units divided among `N` claimant slots.
///
/// Declared (as a `pub static`) by [`supervisor_graph!`](crate::supervisor_graph)
/// for each `divisible` resource name, sized to the nodes and pool members that
/// declare it: one slot each, numbered in declaration order. The protocol:
///
/// 1. Something [`provide`](Self::provide)s the capacity — `main` for a fixed
///    budget, or an allocator node that also names the slot in `provides:` so
///    the budget empties when it stops. A holder whose budget is still
///    unprovided at its gate deadline faults with
///    [`FaultKind::ResourceMissing`](crate::FaultKind::ResourceMissing), like
///    any other slot.
/// 2. Each holder's shell receives a [`Claimant`] bound to its slot and states
///    what it [`want`](Claimant::want)s. The allocator divides the capacity
///    over the wants with a [`BudgetPolicy`] ([`rebalance`](Self::rebalance)),
///    and each holder reads its [`grant`](Claimant::grant), or parks on
///    [`wait_grant_change`](Claimant::wait_grant_change) until it moves.
/// 3. When a holder stops — cleanly, or by missing its shutdown ack — the
///    supervisor [`release`](Self::release)s its slot, so a dead session never
///    strands its share. A parked (`Pause`) holder keeps its claim.
///
/// The budget never chooses a division itself: what "fair" means (equal,
/// proportional, ramped) is the policy's, and *when* to re-divide is the
/// allocator's — usually on [`wait_change`](Self::wait_change), which fires on
/// every want, release and capacity change. Costs `4 + 8N` bytes of atomics,
/// two `Signal`s and `N` single-waker slots per budget: `28 + 16N` bytes on a
/// 32-bit target.
pub struct Budget<const N: usize> {
    /// `0` is "not provided": the gate reads empty.
    capacity: AtomicU32,
    wants: [AtomicU32; N],
    grants: [AtomicU32; N],
    /// The [`ResourceGate`] wake for the supervisor's pre-spawn wait.
    filled: Signal<CriticalSectionRawMutex, ()>,
    /// Holders parked in `wait_grant_change`, at most one per slot.
    claimants: [AtomicWaker; N],
    /// The allocator's wake, single waiter: anything that should trigger a
    /// re-division.
    watch: Signal<CriticalSectionRawMutex, ()>,
}

impl<const N: usize> Default for Budget<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Budget<N> {
    /// An unprovided budget (`const` — it lives in a `static` the macro emits).
    pub const fn new() -> Self {
        const { assert!(N > 0, "a Budget needs at least one claimant slot") };
        Self {
            capacity: AtomicU32::new(0),
            wants: [const { AtomicU32::new(0) }; N],
            grants: [const { AtomicU32::new(0) }; N],
            filled: Signal::new(),
            claimants: [const { AtomicWaker::new() }; N],
            watch: Signal::new(),
        }
    }

    /// Set the capacity and wake both the supervisor's gate wait and the
    /// allocator.
    pub fn provide(&self, capacity: u32) {
        self.capacity.store(capacity, Ordering::Release);
        if capacity > 0 {
            self.filled.signal(());
            crate::__sv_gate_event();
        }
        self.watch.signal(());
    }

    /// The number of claimant slots: what the graph sized the budget to.
    pub const fn slots(&self) -> usize {
        N
    }

    /// The provided capacity, `0` when unprovided.
    pub fn capacity(&self) -> u32 {
        self.capacity.load(Ordering::Acquire)
    }

    /// State slot `slot`'s demand and wake the allocator.
    pub fn want(&self, slot: u8, units: u32) {
        self.wants[usize::from(slot)].store(units, Ordering::Release);
        self.watch.signal(());
    }

    /// Drop slot `slot`'s demand and grant, and wake the allocator: what the
    /// supervisor does when the slot's holder stops.
    pub fn release(&self, slot: u8) {
        let slot = usize::from(slot);
        self.wants[slot].store(0, Ordering::Release);
        self.grants[slot].store(0, Ordering::Release);
        self.watch.signal(());
    }

    /// Slot `slot`'s current grant.
    pub fn grant(&self, slot: u8) -> u32 {
        self.grants[usize::from(slot)].load(Ordering::Acquire)
    }

    /// Slot `slot`'s stated demand.
    pub fn want_of(&self, slot: u8) -> u32 {
        self.wants[usize::from(slot)].load(Ordering::Acquire)
    }

    /// The sum of every slot's grant.
    pub fn total_granted(&self) -> u32 {
        self.grants
            .iter()
            .fold(0u32, |acc, g| acc.saturating_add(g.load(Ordering::Acquire)))
    }

    /// Re-divide the capacity over the current wants with `policy`, publish
    /// the new grants, and wake every holder whose grant moved.
    pub fn rebalance(&self, policy: &impl BudgetPolicy, now: Instant) -> Option<Instant> {
        let capacity = self.capacity();
        let wants: [u32; N] = core::array::from_fn(|i| self.wants[i].load(Ordering::Acquire));
        let mut grants: [u32; N] = core::array::from_fn(|i| self.grants[i].load(Ordering::Acquire));
        let next = policy.divide(capacity, &wants, &mut grants, now);
        for (i, g) in grants.iter().enumerate() {
            if self.grants[i].swap(*g, Ordering::AcqRel) != *g {
                self.claimants[i].wake();
            }
        }
        let stale = self.capacity() != capacity
            || wants
                .iter()
                .zip(&self.wants)
                .any(|(seen, cur)| cur.load(Ordering::Acquire) != *seen);
        if stale {
            self.watch.signal(());
        }
        next
    }

    /// The allocator's wait: resolves after any want, release or capacity
    /// change since the last wait (latching, single waiter).
    pub async fn wait_change(&self) {
        self.watch.wait().await;
    }

    /// The handle a holder of `slot` claims through. Emitted by the macro
    /// into the holder's task shell; hand-built nodes may call it directly.
    pub fn claimant(&'static self, slot: u8) -> Claimant {
        Claimant { budget: self, slot }
    }

    fn wake_claimants(&self) {
        for w in &self.claimants {
            w.wake();
        }
    }
}

impl<const N: usize> ResourceGate for Budget<N> {
    fn is_filled(&self) -> bool {
        self.capacity() > 0
    }

    fn filled_signal(&self) -> &Signal<CriticalSectionRawMutex, ()> {
        &self.filled
    }

    /// Empty the budget: no capacity, no grants. Holders are woken so a loop
    /// parked on its grant sees the zero.
    fn clear(&self) {
        self.capacity.store(0, Ordering::Release);
        for g in &self.grants {
            g.store(0, Ordering::Release);
        }
        self.filled.reset();
        self.wake_claimants();
        self.watch.signal(());
    }
}

/// The object-safe view of a [`Budget`] a [`Claimant`] and a node's claims
/// table go through, so neither names `N`.
pub trait Divisible: Sync {
    /// State `slot`'s demand.
    fn want(&self, slot: u8, units: u32);
    /// Drop `slot`'s demand and grant.
    fn release(&self, slot: u8);
    /// `slot`'s current grant.
    fn grant(&self, slot: u8) -> u32;
    /// Park `waker` until the next rebalance that moves `slot`'s grant.
    fn register(&self, slot: u8, waker: &Waker);
}

impl<const N: usize> Divisible for Budget<N> {
    fn want(&self, slot: u8, units: u32) {
        Budget::want(self, slot, units);
    }

    fn release(&self, slot: u8) {
        Budget::release(self, slot);
    }

    fn grant(&self, slot: u8) -> u32 {
        Budget::grant(self, slot)
    }

    fn register(&self, slot: u8, waker: &Waker) {
        self.claimants[usize::from(slot)].register(waker);
    }
}

/// A holder's handle on one slot of a [`Budget`]: what a `divisible` entry
/// hands the worker. `Copy`, so a worker may keep one per loop it runs.
#[derive(Clone, Copy)]
pub struct Claimant {
    budget: &'static dyn Divisible,
    slot: u8,
}

impl Claimant {
    /// State this slot's demand.
    pub fn want(&self, units: u32) {
        self.budget.want(self.slot, units);
    }

    /// Give the share back early. The supervisor does this when the holder
    /// stops; a holder that is done with the budget mid-run does it itself.
    pub fn release(&self) {
        self.budget.release(self.slot);
    }

    /// This slot's current grant.
    pub fn grant(&self) -> u32 {
        self.budget.grant(self.slot)
    }

    /// Wait until the grant differs from `seen`, then return it.
    /// Uses a check/register/recheck to avoid missing a rebalance between the
    /// load and the park. Only one waiter may park per slot; use your own
    /// `Claimant`.
    pub async fn wait_grant_change(&self, seen: u32) -> u32 {
        core::future::poll_fn(|cx| {
            let g = self.grant();
            if g != seen {
                return Poll::Ready(g);
            }
            self.budget.register(self.slot, cx.waker());
            let g = self.grant();
            if g != seen {
                Poll::Ready(g)
            } else {
                Poll::Pending
            }
        })
        .await
    }

    /// The slot this handle is bound to.
    pub const fn slot(&self) -> u8 {
        self.slot
    }
}

/// How a [`Budget`]'s capacity is divided over its holders' wants.
///
/// `&self`, like [`ScalingPolicy`](crate::ScalingPolicy): a policy that keeps
/// state (a ramp deadline) holds it in interior mutability so the impl can
/// live in a `static`.
pub trait BudgetPolicy {
    /// Write the new grants into `grants`, which arrives holding the previous
    /// division. Return when to run again while the division is converging,
    /// `None` once it is settled.
    fn divide(
        &self,
        capacity: u32,
        wants: &[u32],
        grants: &mut [u32],
        now: Instant,
    ) -> Option<Instant>;
}

/// Every holder gets its want when the capacity covers the sum; otherwise
/// the capacity is split in proportion to the wants, with the integer
/// remainder handed one unit at a time to the lowest slots that can use it.
/// Changes apply immediately in both directions.
pub struct FairShare;

impl FairShare {
    fn targets(capacity: u32, wants: &[u32], mut visit: impl FnMut(usize, u32)) {
        let total: u64 = wants.iter().map(|w| u64::from(*w)).sum();
        if total <= u64::from(capacity) {
            for (i, w) in wants.iter().enumerate() {
                visit(i, *w);
            }
            return;
        }
        let capacity = u64::from(capacity);
        let floor = |w: &u32| (capacity * u64::from(*w) / total) as u32;
        let used: u64 = wants.iter().map(|w| u64::from(floor(w))).sum();
        let mut left = capacity - used;
        for (i, w) in wants.iter().enumerate() {
            let mut t = floor(w);
            if left > 0 && t < *w {
                t += 1;
                left -= 1;
            }
            visit(i, t);
        }
    }
}

impl BudgetPolicy for FairShare {
    fn divide(
        &self,
        capacity: u32,
        wants: &[u32],
        grants: &mut [u32],
        _now: Instant,
    ) -> Option<Instant> {
        Self::targets(capacity, wants, |i, t| grants[i] = t);
        None
    }
}

/// [`FairShare`]'s division, applied asymmetrically: a cut lands at once, an
/// increase is ramped at most `step` units per `interval`. The safety shape
/// of a shared power or bandwidth budget — a holder must never be granted
/// more than the budget can carry, so reductions cannot wait, while a holder
/// drawing more can wait for the others to have backed off.
pub struct ShrinkFastGrowSlow {
    step: u32,
    interval: Duration,
    /// The earliest instant the next increase may land.
    next: Mutex<CriticalSectionRawMutex, Cell<Option<Instant>>>,
}

impl ShrinkFastGrowSlow {
    /// A policy raising any grant by at most `step` units per `interval`.
    pub const fn new(step: u32, interval: Duration) -> Self {
        Self {
            step,
            interval,
            next: Mutex::new(Cell::new(None)),
        }
    }
}

impl BudgetPolicy for ShrinkFastGrowSlow {
    fn divide(
        &self,
        capacity: u32,
        wants: &[u32],
        grants: &mut [u32],
        now: Instant,
    ) -> Option<Instant> {
        let due = self.next.lock(|c| c.get()).is_none_or(|t| now >= t);
        let mut converging = false;
        FairShare::targets(capacity, wants, |i, t| {
            let g = &mut grants[i];
            if t < *g {
                *g = t;
            } else if t > *g {
                if due {
                    *g = (*g).saturating_add(self.step).min(t);
                }
                converging |= *g < t;
            }
        });
        let next = if converging {
            let at = if due {
                now + self.interval
            } else {
                self.next.lock(|c| c.get()).unwrap_or(now)
            };
            Some(at)
        } else {
            None
        };
        self.next.lock(|c| c.set(next));
        next
    }
}
