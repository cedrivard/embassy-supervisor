
use embassy_supervisor::{TaskNode, dataflow, supervisor_graph};

pub static OUT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static ELSEWHERE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[dataflow]
async fn w(node: &'static TaskNode) {
    node.put(&crate::OUT, 1);
}

supervisor_graph! {
    node A = Terminate, deps: [], task: w, discover,
        writes: [crate::ELSEWHERE observed beat];
}

fn main() {}
