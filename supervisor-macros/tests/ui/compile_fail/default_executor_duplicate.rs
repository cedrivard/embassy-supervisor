
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    default executor THREAD;
    default executor OTHER;
    node A = Terminate, deps: [], task: worker;
}

fn main() {}
