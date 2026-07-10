//! Every declaration of a `shared` slot is the SAME static, so re-declaring it
//! with different kind markers or a different type is a contradiction.

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
        resources: [H: shared OtherHandle]; // type differs from A's declaration
}

fn main() {}
