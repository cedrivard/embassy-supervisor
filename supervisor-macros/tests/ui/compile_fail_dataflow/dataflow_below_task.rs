
use embassy_supervisor::{TaskNode, dataflow, supervisor_graph};

pub static LINK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

// The task macro expands first and hands `#[dataflow]` its spawner fn, whose
// body calls no verb: the order must be the other way round.
#[embassy_executor::task]
#[dataflow]
async fn net(node: &'static TaskNode) {
    node.put(&crate::LINK, 1);
}

// The graph still finds `net`'s tables: the one error does not cascade.
supervisor_graph! {
    node NET = Terminate, spawn: net, discover;
}

fn main() {}
