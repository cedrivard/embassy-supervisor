
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    pool P = [Terminate, OnDemand], deps: [],
        spawn: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 3;
}

fn main() {}
