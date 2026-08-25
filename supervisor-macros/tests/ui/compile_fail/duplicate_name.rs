
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    node A = Pause, deps: [];
}

fn main() {}
