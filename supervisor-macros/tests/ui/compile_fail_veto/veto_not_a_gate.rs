use std::sync::atomic::AtomicU32;

use embassy_supervisor::{TaskNode, supervisor_graph};

pub static TRIP: AtomicU32 = AtomicU32::new(0);

async fn protector(_node: &'static TaskNode) {}

supervisor_graph! {
    node OC = Terminate, deps: [], task: protector, writes: [crate::TRIP veto];
}

fn main() {}
