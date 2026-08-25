
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [], task: |_s| Ok(());
}

fn main() {}
