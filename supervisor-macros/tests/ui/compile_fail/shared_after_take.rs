
use embassy_supervisor::{TaskNode, supervisor_graph};

#[derive(Clone, Copy)]
struct Handle;

async fn taker(_node: &'static TaskNode, _h: &mut Handle) {}
async fn sharer(_node: &'static TaskNode, _h: Handle) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: taker, resources: [H: Handle];
    node B = Terminate, deps: [], task: sharer, resources: [H: shared Handle];
}

fn main() {}
