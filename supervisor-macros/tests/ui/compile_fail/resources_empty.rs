
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node W = Terminate, deps: [], task: worker, resources: [];
}

fn main() {}
