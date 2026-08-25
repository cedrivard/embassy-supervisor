
use embassy_supervisor::{TaskNode, dataflow, supervisor_graph};

pub static OUT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[dataflow]
fn set_out(node: &'static TaskNode, v: u32) {
    node.put(&crate::OUT, v);
}

async fn f(_n: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: f, ready_on_write,
        dataflow: [crate::set_out];
}

fn main() {}
