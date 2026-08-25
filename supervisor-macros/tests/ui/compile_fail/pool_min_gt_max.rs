
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    pool P = [Terminate, OnDemand, OnDemand], deps: [],
        spawn: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 3, max: 2;
}

fn main() {}
