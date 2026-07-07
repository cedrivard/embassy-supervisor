//! Duplicate resource slot names — within one node and across nodes. Slots are
//! emitted as statics, so every name must be unique across the whole graph.

use embassy_supervisor::{TaskNode, supervisor_graph};

struct FakeLed;
struct FakeUart;

async fn worker(_node: &'static TaskNode, _a: &mut FakeLed, _b: &mut FakeUart) {}

supervisor_graph! {
    node W = Terminate, deps: [], task: worker,
        resources: [LED: FakeLed, LED: FakeUart];
}

fn main() {}
