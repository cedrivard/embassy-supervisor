//! `exit:` is rejected on `pool` — K members share one generated shell, so
//! per-member exit values would need per-member storage.

use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) -> u32 {
    0
}

supervisor_graph! {
    pool WORKERS = [Terminate, OnDemand], deps: [], task: worker,
        exit: u32,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
}

fn main() {}
