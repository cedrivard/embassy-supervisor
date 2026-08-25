
use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task(pool_size = 2)]
async fn member_task(_node: &'static TaskNode) {}

supervisor_graph! {
    pool WORKERS = [Terminate, OnDemand], deps: [],
        spawn: member_task,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2, cancel;
}

fn main() {}
