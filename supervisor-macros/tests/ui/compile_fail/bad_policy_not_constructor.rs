
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    pool P = [Terminate], deps: [],
        spawn: worker,
        policy: make_policy(dur),
        min: 1, max: 1;
}

fn main() {}
