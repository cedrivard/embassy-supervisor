use crate::{Coupling, TaskNode};

#[cfg(feature = "macros")]
pub use embassy_supervisor_macros::dataflow;

#[cfg(feature = "macros")]
pub use embassy_supervisor_macros::dataflow_bundle;

pub use embassy_supervisor_observe::{Sink, Source};

/// A typed handle to a signal declared in a `reads:`/`writes:` list.
pub struct Sig<T: ?Sized + 'static> {
    /// The call site's entry: path text plus the type-erased identity.
    pub entry: &'static Coupling,
    /// The signal, concretely typed.
    pub target: &'static T,
}

// By hand: a derive would require `T: Copy`, but both fields are references.
impl<T: ?Sized + 'static> Clone for Sig<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized + 'static> Copy for Sig<T> {}

impl TaskNode {
    /// Write `v` into the signal `s`.
    pub fn put<T: Sink + Sync>(&self, s: Sig<T>, v: T::Item) {
        s.target.put(v);
    }

    #[cfg(feature = "liveness")]
    /// Write `v` into `s` and record a heartbeat.
    pub fn beat_put<T: Sink + Sync>(&self, s: Sig<T>, v: T::Item) {
        self.beat();
        s.target.put(v);
    }

    /// Read and return a snapshot of the signal `s`.
    pub fn get<T: Source + Sync>(&self, s: Sig<T>) -> T::Item {
        s.target.get()
    }

    /// Borrow the signal target for a direct write, bypassing the [`Sink`] trait.
    pub fn writer<T: Sync + ?Sized>(&self, s: Sig<T>) -> &'static T {
        s.target
    }

    /// [`writer`](Self::writer) that is also the node's sign of life: it records a
    /// [`beat`](Self::beat) before handing the target back, so the write that
    /// proves progress needs no separate `beat()`.
    #[cfg(feature = "liveness")]
    pub fn beat_writer<T: Sync + ?Sized>(&self, s: Sig<T>) -> &'static T {
        self.beat();
        s.target
    }

    /// Hand the signal back for a read — the wiring point for consuming reads,
    /// which need per-consumer handle state no shared static can carry:
    /// `node.reader(&ESTIMATE).receiver()`. A pass-through, like
    /// [`get`](Self::get).
    pub fn reader<T: Sync + ?Sized>(&self, s: Sig<T>) -> &'static T {
        s.target
    }
}
