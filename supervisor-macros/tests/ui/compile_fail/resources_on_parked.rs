//! `resources:` on a parked node (no `spawn:`/`task:`) — the application spawns
//! a parked node itself, so the macro has no glue to thread the resources through.

use embassy_supervisor::supervisor_graph;

struct FakeLed;

supervisor_graph! {
    node PARKED = Terminate, deps: [], resources: [LED: FakeLed];
}

fn main() {}
