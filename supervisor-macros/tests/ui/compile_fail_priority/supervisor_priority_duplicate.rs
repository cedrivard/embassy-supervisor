use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    supervisor priority: 3;
    supervisor priority: 4;
    node A = Terminate, deps: [], task: worker;
}

fn main() {}
