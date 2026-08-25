
use embassy_supervisor::{Supervisor, TaskNode, dataflow, supervisor_graph};

pub static ALWAYS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static AFTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "nope")]
pub static GATED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[dataflow]
async fn worker(node: &'static TaskNode) {
    // Gated FIRST, so every later entry's index shifts when it compiles out.
    #[cfg(feature = "nope")]
    {
        node.put(&crate::GATED, 1);
        let _ = node.get(&crate::GATED);
    }
    node.put(&crate::ALWAYS, 2);
    #[cfg(feature = "nope")]
    if false {
        let _ = node.get(&crate::GATED);
    }
    node.put(&crate::AFTER, 3);
    #[cfg(feature = "nope")]
    node.put(&crate::ALWAYS, 4);
    node.ack_dropped();
}

supervisor_graph! {
    node A = Terminate, task: worker, discover;
}

fn main() {
    let _sup = Supervisor::new(&GRAPH);
    let writes = A.writes()[0];
    assert_eq!(writes.len(), 2, "the gated write compiled out");
    assert_eq!(writes[0].name(), "crate::ALWAYS", "index shifted correctly");
    assert_eq!(writes[1].name(), "crate::AFTER", "gate scope ended at its statement");
    assert_eq!(A.reads()[0].len(), 0, "both gated reads compiled out");
}
