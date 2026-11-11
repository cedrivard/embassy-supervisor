
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode, _v: &mut u32) {}

// `P` is parked: the application spawns it on an executor of its choosing,
// so the graph cannot hold the provider of a `local` slot to the slot's tier.
supervisor_graph! {
    node P = Terminate, deps: [], provides: [BLOB];
    node N = Terminate, deps: [P], task: worker, resources: [BLOB: local u32];
}

fn main() {}
