
use embassy_supervisor::supervisor_graph;

struct FakeLed;

supervisor_graph! {
    node PARKED = Terminate, deps: [], resources: [LED: FakeLed];
}

fn main() {}
