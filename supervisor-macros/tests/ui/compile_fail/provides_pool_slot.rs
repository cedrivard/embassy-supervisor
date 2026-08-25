
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}
async fn provider(_node: &'static TaskNode) {}

pub struct Buf;

supervisor_graph! {
    pool CREW = [Terminate, Terminate], deps: [],
        task: worker,
        resources: [BUF: Buf],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
    node PROVIDER = Terminate, deps: [], task: provider, provides: [BUF];
}

fn main() {}
