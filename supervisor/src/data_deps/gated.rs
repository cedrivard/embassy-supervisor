use crate::{Coupling, Sig, TaskNode};

#[allow(async_fn_in_trait)]
/// A signal whose producer must be running and ready before a reader can use it.
pub trait Gated {
    /// Ensure the producer of `entry` is serving from the perspective of `caller`.
    async fn ensure(&'static self, caller: &'static TaskNode, entry: &'static Coupling);
}

impl TaskNode {
    /// [`reader`](Self::reader) through the signal's gate: run its
    pub async fn open<T: Gated + Sync + ?Sized>(&'static self, s: Sig<T>) -> &'static T {
        s.target.ensure(self, s.entry).await;
        s.target
    }
}
