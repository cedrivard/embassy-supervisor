
use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn probe_task(_node: &'static TaskNode) {}

supervisor_graph! {
    node PROBE = Terminate, deps: [], spawn: probe_task, exit: u32;
}

fn main() {}
