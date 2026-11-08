
use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    pool P = [Terminate], deps: [], task: worker, resources: [],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)), min: 1, max: 1;
}

fn main() {}
