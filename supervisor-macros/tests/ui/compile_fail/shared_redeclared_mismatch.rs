//! Every declaration of a `shared` slot is the SAME static, so re-declaring it
//! with different kind markers or a different type is a contradiction.

use embassy_supervisor::{TaskNode, supervisor_graph};

#[derive(Clone, Copy)]
struct Handle;

async fn worker(_node: &'static TaskNode, _h: Handle) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: worker,
        resources: [H: shared Handle];
    node B = Terminate, deps: [], task: worker,
        resources: [H: shared local Handle]; // kinds differ from A's declaration
}

fn main() {}
