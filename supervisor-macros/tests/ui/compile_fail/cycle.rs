
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [B];
    node B = Terminate, deps: [A];
}

fn main() {}
