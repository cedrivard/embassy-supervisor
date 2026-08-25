
use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn real_task(_node: &'static TaskNode) {}

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], spawn: real_task, task: worker;
}

fn main() {}
