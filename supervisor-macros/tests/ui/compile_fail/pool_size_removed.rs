
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: worker, pool_size: 2;
}

fn main() {}
