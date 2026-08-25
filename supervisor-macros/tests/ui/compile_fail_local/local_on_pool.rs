
use embassy_supervisor::{TaskNode, supervisor_graph};

struct Handle;

async fn worker(_node: &'static TaskNode, _h: Handle) {}

supervisor_graph! {
    pool CRUNCH = [Terminate, OnDemand], deps: [],
        task: worker,
        resources: [H: local consume Handle],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {}
