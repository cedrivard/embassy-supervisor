use embassy_supervisor::{TaskNode, supervisor_graph};

#[derive(Clone, Copy)]
struct Bus;

async fn worker(_node: &'static TaskNode, _bus: Bus) {}

supervisor_graph! {
    executor HIGH;
    node MODBUS = Terminate, deps: [], task: worker, resources: [BUS: shared serialized Bus];
    node LOGGER = Terminate, deps: [], executor: HIGH, task: worker,
        resources: [BUS: shared serialized Bus];
}

fn main() {}
