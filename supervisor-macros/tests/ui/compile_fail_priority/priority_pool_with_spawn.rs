use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task(pool_size = 2)]
async fn member(_node: &'static TaskNode) {}

supervisor_graph! {
    pool P = [Terminate, OnDemand], deps: [], priority: 3, spawn: member,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {}
