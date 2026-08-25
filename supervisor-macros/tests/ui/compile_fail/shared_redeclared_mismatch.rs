
use embassy_supervisor::{TaskNode, supervisor_graph};

#[derive(Clone, Copy)]
struct Handle;
#[derive(Clone, Copy)]
struct OtherHandle;

async fn worker(_node: &'static TaskNode, _h: Handle) {}
async fn other_worker(_node: &'static TaskNode, _h: OtherHandle) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: worker,
        resources: [H: shared Handle];
    node B = Terminate, deps: [], task: other_worker,
        resources: [H: shared OtherHandle]; 
}

fn main() {}
