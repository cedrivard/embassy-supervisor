
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps [];
}

fn main() {}
