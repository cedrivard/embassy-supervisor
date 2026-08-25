
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn net_worker(_node: &'static TaskNode) {}
async fn http_worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node NET = Terminate, deps: [], task: net_worker;
    node HTTP = Terminate, deps: [NET rdy], task: http_worker;
}

fn main() {}
