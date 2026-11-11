//! Declared dataflow coupling: the signals a node reads and writes.

// ─── Declared dataflow coupling ───────────────────────────────────────────
//
// `deps:` says "spawn me after that". It is consumed once and says nothing
// about the relationship that holds for the rest of the program's life: the
// signals a task reads and writes. `reads:`/`writes:` declare *that* relation,
// which outlives the spawn the `deps:` edge described.

/// A signal a node declares it reads or writes.
///
/// Implemented for every `Sync` type by a blanket impl, so no consumer ever
/// writes one: the graph's `reads:`/`writes:` clauses coerce the named statics
/// to `&'static dyn CouplingPoint` for you. The trait carries **no methods** —
/// the supervisor neither knows nor cares what a signal is. Its only purpose is
/// to type-erase heterogeneous statics into one slice, so identity is the
/// static's address and nothing else.
#[cfg(feature = "coupling")]
pub trait CouplingPoint: Sync {}

#[cfg(feature = "coupling")]
impl<T: Sync + ?Sized> CouplingPoint for T {}

#[cfg(feature = "coupling-observe")]
pub use embassy_supervisor_observe::Observable;

#[cfg(feature = "coupling-observe")]
#[derive(Clone, Copy)]
/// A callable that returns a signal's change count for observation.
pub struct Observer {
    count: fn() -> u32,
}

#[cfg(feature = "coupling-observe")]
impl Observer {
    /// Wrap a function that returns the signal's current change count.
    pub const fn new(count: fn() -> u32) -> Self {
        Self { count }
    }

    /// Return the current change count.
    pub fn count(&self) -> u32 {
        (self.count)()
    }
}

#[cfg(feature = "coupling")]
#[derive(Clone, Copy)]
/// A declared read/write coupling between a node and a signal.
pub struct Coupling {
    name: &'static str,
    point: &'static dyn CouplingPoint,
    #[cfg(feature = "coupling-observe")]
    observe: Option<Observer>,
    #[cfg(feature = "coupling-observe")]
    beat: bool,
    #[cfg(feature = "veto")]
    veto: Option<u8>,
}

#[cfg(feature = "coupling")]
impl Coupling {
    /// Create a coupling with the given path name and wiring point.
    pub const fn new(name: &'static str, point: &'static dyn CouplingPoint) -> Self {
        Self {
            name,
            point,
            #[cfg(feature = "coupling-observe")]
            observe: None,
            #[cfg(feature = "coupling-observe")]
            beat: false,
            #[cfg(feature = "veto")]
            veto: None,
        }
    }

    #[cfg(feature = "veto")]
    /// Mark this coupling as a `veto` write holding contributor `slot`.
    pub const fn veto(mut self, slot: u8) -> Self {
        self.veto = Some(slot);
        self
    }

    /// This writer's contributor slot, if the entry carries the `veto` marker.
    #[cfg(feature = "veto")]
    pub const fn veto_slot(&self) -> Option<u8> {
        self.veto
    }

    #[cfg(feature = "coupling-observe")]
    /// Mark this coupling as observed with the given observer.
    pub const fn observed(mut self, observer: Observer) -> Self {
        self.observe = Some(observer);
        self
    }

    #[cfg(feature = "coupling-observe")]
    /// Mark this coupling as feeding a heartbeat.
    pub const fn beat(mut self) -> Self {
        self.beat = true;
        self
    }

    /// Return the path name of this coupling.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// This entry's accessor, if it carries the `observed` marker.
    #[cfg(feature = "coupling-observe")]
    pub const fn observer(&self) -> Option<Observer> {
        self.observe
    }

    /// Return whether this coupling feeds a heartbeat.
    #[cfg(feature = "coupling-observe")]
    pub const fn beats(&self) -> bool {
        self.beat
    }
}

#[cfg(feature = "coupling")]
impl core::fmt::Debug for Coupling {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name)
    }
}

/// Do two declarations refer to the same static? Compares **data pointers
/// only** (`ptr::addr_eq`): the same static reached through two different
/// generic instantiations can carry different vtables, and a plain
/// `ptr::eq` on trait objects would compare those too and miss the match.
#[cfg(feature = "coupling")]
pub(crate) fn same_signal(a: &Coupling, b: &Coupling) -> bool {
    core::ptr::addr_eq(
        a.point as *const dyn CouplingPoint,
        b.point as *const dyn CouplingPoint,
    )
}

/// Does `table` carry an entry whose declared path ends in the same segment as
/// `name`? Emitted by `supervisor_graph!` as a const assertion behind a marked
/// entry beside `discover`: the entry may only add a marker to a signal the
/// task fn already accesses, and this is what checks it before anything runs.
///
/// **By name, and only the last segment**, because a const context cannot
/// compare addresses: `ptr::addr_eq` is not `const`, and a pointer has no
/// integer value at compile time. The declaration and the call site usually
/// spell the same static differently (`crate::signals::EST` against an aliased
/// `s::EST`), so the tail is the most that can be matched. Two consequences,
/// both benign: a signal reached through a renaming re-export fails the check
/// though it is legitimate, and two distinct signals sharing a final segment
/// pass it. A marker that lands on the wrong static costs nothing silently —
/// no verb write matches it, so the node simply never beats and the liveness
/// monitor reports it stale.
#[doc(hidden)]
#[cfg(feature = "coupling")]
pub const fn __sv_tail_declared(table: &[Coupling], name: &str) -> bool {
    let mut i = 0;
    while i < table.len() {
        if tail_eq(table[i].name, name) {
            return true;
        }
        i += 1;
    }
    false
}

/// Byte offset just past the last `::` in `s`, or 0.
#[cfg(feature = "coupling")]
const fn tail_start(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = b.len();
    while i >= 2 {
        if b[i - 1] == b':' && b[i - 2] == b':' {
            return i;
        }
        i -= 1;
    }
    0
}

/// Do two paths end in the same segment, index and all (`ARR[1]`)?
#[cfg(feature = "coupling")]
const fn tail_eq(a: &str, b: &str) -> bool {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (ai, bi) = (tail_start(a), tail_start(b));
    if ab.len() - ai != bb.len() - bi {
        return false;
    }
    let mut k = 0;
    while ai + k < ab.len() {
        if ab[ai + k] != bb[bi + k] {
            return false;
        }
        k += 1;
    }
    true
}
