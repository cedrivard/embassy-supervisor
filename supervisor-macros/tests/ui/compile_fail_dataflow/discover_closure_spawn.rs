
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [], spawn: |_sp| Ok(()), discover;
}

fn main() {}
