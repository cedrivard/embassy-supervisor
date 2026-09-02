use embassy_supervisor::{TaskNode, VetoGate, supervisor_graph};

pub static TRIP: VetoGate<2> = VetoGate::new();

async fn protector(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: protector, writes: [crate::TRIP veto];
    node B = Terminate, deps: [], task: protector, writes: [TRIP veto];
}

fn main() {}
