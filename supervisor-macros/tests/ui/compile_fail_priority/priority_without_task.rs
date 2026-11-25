use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn hand(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], priority: 3, spawn: hand;
}

fn main() {}
