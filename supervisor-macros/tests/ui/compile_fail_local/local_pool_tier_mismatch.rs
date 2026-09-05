
use embassy_supervisor::{TaskNode, supervisor_graph};

type LocalHandle = (u32, *const ());

async fn fan_worker(_node: &'static TaskNode, _s: LocalHandle) {}
async fn consumer(_node: &'static TaskNode, _s: LocalHandle) {}

// Two declarations of one `shared local` slot on two executors.
supervisor_graph! {
    executor HIGH;
    pool FANS = [Terminate, OnDemand], deps: [], executor: HIGH,
        task: fan_worker,
        resources: [S: shared local LocalHandle],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
    node USER = Terminate, deps: [],
        task: consumer,
        resources: [S: shared local LocalHandle];
}

fn main() {}
