
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    node B = Terminate, deps: [A, A];
}

fn main() {}
