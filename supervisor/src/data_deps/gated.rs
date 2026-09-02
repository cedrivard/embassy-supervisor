use core::ops::Deref;

use crate::{Coupling, Sig, TaskNode};

#[allow(async_fn_in_trait)]
/// A signal whose producer must be running and ready before a reader can use it.
pub trait Gated {
    /// Handle returned by [`open`](TaskNode::open) after the gate passes.
    /// Counting gates return a guard; simple gates return `&'static Self`.
    type Handle: Deref;

    /// Admit a reader before [`ensure`](Self::ensure) runs, so the producer
    /// sees an incoming reader and a cancelled `open` can roll back.
    fn admit(&'static self) -> Self::Handle;

    /// Ensure the producer of `entry` is serving from the perspective of `caller`.
    async fn ensure(&'static self, caller: &'static TaskNode, entry: &'static Coupling);
}

impl TaskNode {
    /// Open a gated signal: admit this node, wait for the producer, then
    /// return the gate's handle. For [`Backed`](crate::Backed) this is an
    /// [`Open`](crate::Open) guard; dropping it lets the producer retire.
    pub async fn open<T: Gated + Sync + ?Sized>(&'static self, s: Sig<T>) -> T::Handle {
        let handle = s.target.admit();
        s.target.ensure(self, s.entry).await;
        handle
    }
}
