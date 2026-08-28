// A pool's value-level clauses (`slot_timeout:` / `ack_timeout:`) take the
// same `#[cfg(...)]` gates as a node's, applied to every member.
use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    pool P = [Terminate, OnDemand], deps: [], task: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2,
        #[cfg(all())] slot_timeout: 500,
        #[cfg(any())] ack_timeout: 900;
}

fn main() {
    assert_eq!(P[0].slot_timeout(), embassy_time::Duration::from_millis(500));
    assert_eq!(P[1].slot_timeout(), embassy_time::Duration::from_millis(500));
}
