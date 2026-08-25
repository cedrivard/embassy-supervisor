
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    pool P = [Terminate], deps: [A],
        spawn: worker,
        min: 1, max: 1;
}

fn main() {}
