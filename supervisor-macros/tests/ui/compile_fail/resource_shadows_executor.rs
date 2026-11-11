
use embassy_supervisor::{TaskNode, supervisor_graph};

struct Bus;

async fn worker(_node: &'static TaskNode, _bus: &mut Bus) {}

supervisor_graph! {
    executor BUS;
    node A = Terminate, deps: [], task: worker, resources: [BUS: Bus];
}

fn main() {}
