
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node Net = Terminate, deps: [];
    node NET = Terminate, deps: [];
}

fn main() {}
