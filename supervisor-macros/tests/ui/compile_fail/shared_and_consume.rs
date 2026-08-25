
use embassy_supervisor::{TaskNode, supervisor_graph};

#[derive(Clone, Copy)]
struct Handle;

async fn worker(_node: &'static TaskNode, _h: Handle) {}

supervisor_graph! {
    node N = Terminate, deps: [], task: worker,
        resources: [H: shared consume Handle];
}

fn main() {}
