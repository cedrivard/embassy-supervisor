
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    pool W = [Terminate, Terminate], deps: [], spawn: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
    node W0 = Terminate, deps: [];
}

fn main() {}
