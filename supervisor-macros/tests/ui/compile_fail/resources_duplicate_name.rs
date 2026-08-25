
use embassy_supervisor::{TaskNode, supervisor_graph};

struct FakeLed;
struct FakeUart;

async fn worker(_node: &'static TaskNode, _a: &mut FakeLed, _b: &mut FakeUart) {}

supervisor_graph! {
    node W = Terminate, deps: [], task: worker,
        resources: [LED: FakeLed, LED: FakeUart];
}

fn main() {}
