use embassy_supervisor::{Claimant, TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode, _p: Claimant) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: worker, pool_size: 2,
        resources: [P: divisible];
}

fn main() {}
