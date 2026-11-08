
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) -> u32 {
    0
}

supervisor_graph! {
    node X = Terminate, deps: [], task: worker, exit: u32;
    node X_EXIT = Terminate, deps: [];
}

fn main() {}
