use embassy_supervisor::{TaskNode, VetoGate, supervisor_graph};

pub static TRIP: VetoGate<2> = VetoGate::new();

async fn breaker(_node: &'static TaskNode) {}

supervisor_graph! {
    node BREAKER = Terminate, deps: [], task: breaker, reads: [crate::TRIP veto];
}

fn main() {}
