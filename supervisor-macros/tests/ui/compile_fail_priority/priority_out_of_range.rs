use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], priority: 256, task: worker;
}

fn main() {}
