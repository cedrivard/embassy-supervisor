
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    executor HIGH;
    node A = Terminate, deps: [], executor: TYPO, spawn: worker;
}

fn main() {}
