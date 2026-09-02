//! A distributed veto: one gate several writers may assert, none of which
//! owns its release.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use portable_atomic::{AtomicU32, Ordering};

use crate::{Sig, TaskNode, same_signal};

/// A gate that is asserted while **any** of its `N` contributors holds it
/// and released only once **all** of them have let go.
///
/// The shape of a trip matrix or a safety chain: every protection function
/// writing the gate may force the safe state, and no single one can clear
/// another's contribution. Writers declare `writes: [crate::TRIP veto]`; the
/// graph numbers them — one slot per node, one per pool member — and proves
/// at compile time that the target is a `VetoGate` with room for them all.
/// A writer takes its handle with [`TaskNode::veto`] and asserts or releases
/// its own bit; the actuator reads the gate through `reads:` and parks on
/// [`wait_asserted`](Self::wait_asserted) / [`wait_released`](Self::wait_released).
///
/// A contributor's bit **stays asserted when its writer stops**: a dead
/// protection function keeps the trip. Release is explicit — the writer's
/// own [`Veto::release`], or the application through
/// [`release_slot`](Self::release_slot). Costs one `AtomicU32` and one
/// `Signal` per gate (plus a transition counter under `coupling-observe`).
pub struct VetoGate<const N: usize> {
    bits: AtomicU32,
    /// Bumped on every flip of the asserted state, so a polled token moves
    /// even when the same contributor asserts, releases and asserts again.
    #[cfg(feature = "coupling-observe")]
    seq: AtomicU32,
    /// Single waiter: the actuator parked on a flip.
    changed: Signal<CriticalSectionRawMutex, ()>,
}

impl<const N: usize> Default for VetoGate<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> VetoGate<N> {
    /// A released gate (`const` — it lives in a `static`).
    pub const fn new() -> Self {
        const { assert!(N >= 1 && N <= 32, "a VetoGate holds 1 to 32 contributors") };
        Self {
            bits: AtomicU32::new(0),
            #[cfg(feature = "coupling-observe")]
            seq: AtomicU32::new(0),
            changed: Signal::new(),
        }
    }

    /// Is any contributor holding the gate?
    pub fn is_asserted(&self) -> bool {
        self.contributors() != 0
    }

    /// The contributors currently asserting, one bit per slot.
    pub fn contributors(&self) -> u32 {
        self.bits.load(Ordering::Acquire)
    }

    /// Assert contributor `slot`. Returns whether the gate flipped from
    /// released to asserted.
    pub fn assert_slot(&self, slot: u8) -> bool {
        let before = self.bits.fetch_or(Self::mask(slot), Ordering::AcqRel);
        let flipped = before == 0;
        if flipped {
            self.flipped();
        }
        flipped
    }

    /// Release contributor `slot`. Returns whether the gate flipped from
    /// asserted to released — only once the last contributor lets go.
    pub fn release_slot(&self, slot: u8) -> bool {
        let mask = Self::mask(slot);
        let before = self.bits.fetch_and(!mask, Ordering::AcqRel);
        let flipped = before == mask;
        if flipped {
            self.flipped();
        }
        flipped
    }

    /// Park until the gate is asserted (returns at once if it already is).
    pub async fn wait_asserted(&self) {
        while !self.is_asserted() {
            self.changed.wait().await;
        }
    }

    /// Park until every contributor has released the gate.
    pub async fn wait_released(&self) {
        while self.is_asserted() {
            self.changed.wait().await;
        }
    }

    fn mask(slot: u8) -> u32 {
        assert!(
            usize::from(slot) < N,
            "veto slot beyond the gate's contributors"
        );
        1 << slot
    }

    fn flipped(&self) {
        #[cfg(feature = "coupling-observe")]
        self.seq.fetch_add(1, Ordering::AcqRel);
        self.changed.signal(());
    }
}

#[cfg(feature = "coupling-observe")]
impl<const N: usize> crate::Observable for VetoGate<N> {
    fn change_token(&self) -> u32 {
        self.seq.load(Ordering::Acquire)
    }
}

/// Emitted by `supervisor_graph!` behind every `veto` target: a type error if
/// the target is not a `VetoGate`, a const-eval failure if the graph declares
/// more `veto` writers than the gate has slots.
#[doc(hidden)]
pub const fn __sv_check_veto<const N: usize>(_: &VetoGate<N>, writers: usize) {
    assert!(
        writers <= N,
        "supervisor: more `veto` writers declared for this gate than it has contributor slots"
    );
}

/// One writer's handle on its slot of a [`VetoGate`], from [`TaskNode::veto`].
#[derive(Clone, Copy)]
pub struct Veto<const N: usize> {
    gate: &'static VetoGate<N>,
    slot: u8,
}

impl<const N: usize> Veto<N> {
    /// Assert this writer's contribution; `true` iff the gate flipped.
    pub fn assert(&self) -> bool {
        self.gate.assert_slot(self.slot)
    }

    /// Release this writer's contribution; `true` iff the gate flipped, which
    /// is only when this was the last contributor holding it.
    pub fn release(&self) -> bool {
        self.gate.release_slot(self.slot)
    }

    /// Is this writer's bit set?
    pub fn is_asserting(&self) -> bool {
        self.gate.contributors() & (1 << self.slot) != 0
    }

    /// This writer's contributor slot.
    pub const fn slot(&self) -> u8 {
        self.slot
    }
}

impl TaskNode {
    /// This node's handle on the gate `s`, bound to the contributor slot the
    /// graph assigned its `writes: [.. veto]` entry. `None` (and a warning)
    /// when no such entry names the gate — a body reaching a gate its
    /// declaration never claimed a slot in.
    pub fn veto<const N: usize>(&self, s: Sig<VetoGate<N>>) -> Option<Veto<N>> {
        let slot = self
            .entries(true)
            .find_map(|e| same_signal(e, s.entry).then(|| e.veto_slot()).flatten());
        let Some(slot) = slot else {
            warn!(
                "supervisor: {} holds no `veto` slot in {}",
                self.name(),
                s.entry.name()
            );
            return None;
        };
        Some(Veto {
            gate: s.target,
            slot,
        })
    }
}
