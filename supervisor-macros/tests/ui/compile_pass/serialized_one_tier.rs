use embassy_supervisor::{TaskNode, supervisor_graph};

#[derive(Clone, Copy)]
struct Bus;

async fn worker(_node: &'static TaskNode, _bus: Bus) {}

supervisor_graph! {
    executor FIELDBUS;
    node MODBUS = Terminate, deps: [], executor: FIELDBUS, task: worker,
        resources: [BUS: shared serialized Bus];
    node METER = Terminate, deps: [], executor: FIELDBUS, task: worker,
        resources: [BUS: shared serialized Bus];
}

fn main() {}
