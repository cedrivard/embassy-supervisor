//! `resources:` is node-only: a ResourceSlot holds ONE value and pool members all
//! run the same worker — they would contend for the single instance.

use embassy_supervisor::{TaskNode, supervisor_graph};

struct FakeLed;

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    pool CRUNCH = [Terminate, OnDemand], deps: [],
        task: worker,
        resources: [LED: FakeLed],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {}
