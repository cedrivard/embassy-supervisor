//! `ready`-marked deps: node dep, pool-as-dep (floor member), pool deps, and a
//! cfg'd ready dep all expand; spawn ordering (DEPS/order) is unchanged.

use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

async fn net_worker(node: &'static TaskNode) {
    node.set_ready();
}
async fn http_worker(_node: &'static TaskNode) {}
async fn probe_worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node NET = Terminate, deps: [], task: net_worker;
    pool HTTP = [Terminate, OnDemand], deps: [NET ready], task: http_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
    node PROBE = Terminate, deps: [HTTP ready, #[cfg(feature = "never")] NET],
        task: probe_worker;
}

fn main() {
    // Readiness is an overlay: the spawn-order table is what it always was.
    assert_eq!(GRAPH.order.len(), 4);
    assert!(!NET.is_ready());
    NET.set_ready();
    assert!(NET.is_ready());
    NET.clear_ready();
    assert!(!NET.is_ready());
}
