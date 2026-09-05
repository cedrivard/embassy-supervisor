
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    #[cfg(feature = "tiers")]
    default executor THREAD;
    node A = Terminate, deps: [], task: worker;
}

fn main() {}
