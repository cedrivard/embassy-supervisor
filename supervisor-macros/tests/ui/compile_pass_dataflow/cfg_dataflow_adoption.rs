
use embassy_supervisor::{TaskNode, dataflow, supervisor_graph};

pub static A_SIG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "nope")]
pub static B_SIG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[dataflow]
pub fn always(node: &'static TaskNode, v: u32) {
    node.put(&crate::A_SIG, v);
}

#[cfg(feature = "nope")]
#[dataflow]
pub fn gated(node: &'static TaskNode, v: u32) {
    node.put(&crate::B_SIG, v);
}

#[dataflow]
async fn worker(node: &'static TaskNode) {
    crate::always(node, 1);
    node.ack_dropped();
}

supervisor_graph! {
    node N = Terminate, task: worker, discover,
        dataflow: [crate::always, #[cfg(feature = "nope")] crate::gated];
}

fn main() {
    // The node binds its own derived table plus the one live adoption; the
    // cfg'd-out entry contributes nothing.
    assert_eq!(N.writes().len(), 2);
}
