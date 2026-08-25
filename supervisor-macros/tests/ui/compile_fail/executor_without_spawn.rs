
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    executor HIGH;
    node A = Pause, deps: [], executor: HIGH;
}

fn main() {}
