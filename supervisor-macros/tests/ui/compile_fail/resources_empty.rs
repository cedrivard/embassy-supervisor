//! An empty `resources:` list is a mistake — drop the clause instead.

use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node W = Terminate, deps: [], task: worker, resources: [];
}

fn main() {}
