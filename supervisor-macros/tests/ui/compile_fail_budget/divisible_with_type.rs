use embassy_supervisor::{Claimant, TaskNode, supervisor_graph};

async fn holder(_node: &'static TaskNode, _power: Claimant) {}

supervisor_graph! {
    node ONE = Terminate, deps: [], task: holder, resources: [POWER: divisible u32];
}

fn main() {}
