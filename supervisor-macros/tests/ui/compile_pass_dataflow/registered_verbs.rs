
use embassy_supervisor::{Sig, Supervisor, TaskNode, dataflow, supervisor_graph};

pub static ESTIMATE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static ARMED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub trait Signals {
    fn subscribe<T: Sync + ?Sized>(&'static self, s: Sig<T>) -> &'static T;
    fn publish(&'static self, s: Sig<core::sync::atomic::AtomicU32>, v: u32);
}

impl Signals for TaskNode {
    fn subscribe<T: Sync + ?Sized>(&'static self, s: Sig<T>) -> &'static T {
        s.target
    }
    fn publish(&'static self, s: Sig<core::sync::atomic::AtomicU32>, v: u32) {
        s.target.store(v, core::sync::atomic::Ordering::Relaxed);
    }
}

#[dataflow(read(subscribe), write(publish))]
async fn worker(node: &'static TaskNode) {
    node.subscribe(&crate::ESTIMATE);
    node.publish(&crate::ARMED, 1);
    node.get(&crate::ESTIMATE);
}

supervisor_graph! {
    node WORKER = Terminate, deps: [], task: worker, discover;
}

fn main() {
    let _ = Supervisor::new(&GRAPH);
    assert_eq!(WORKER.reads().iter().flat_map(|t| t.iter()).count(), 1);
    assert_eq!(WORKER.writes().iter().flat_map(|t| t.iter()).count(), 1);
}
