
use embassy_supervisor::supervisor_graph;

pub static OUT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

async fn f(_n: &'static embassy_supervisor::TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: f, writes: [crate::OUT beat];
}

fn main() {}
