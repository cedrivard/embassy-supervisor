//! `#[cfg(...)]` on a whole pool declaration. A cfg-ed-out pool keeps its member
//! *slots* (the macro can't evaluate cfg), but its `ElasticPool` static and its
//! `GRAPH.pools` entry are gated away, and its `GRAPH.nodes` slot becomes `None`.

use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [];
    #[cfg(any())] // always false
    pool GONE = [Terminate], deps: [A],
        spawn: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 1;
    #[cfg(all())] // always true
    pool HERE = [Terminate], deps: [A],
        spawn: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 1;
}

fn main() {
    // Slots exist for A, GONE's member, HERE's member regardless of cfg.
    assert_eq!(GRAPH.nodes.len(), 3);
    assert!(GRAPH.nodes[0].is_some()); // A
    assert!(GRAPH.nodes[1].is_none()); // GONE member, cfg-ed out
    assert!(GRAPH.nodes[2].is_some()); // HERE member
    // Only the surviving pool registers in GRAPH.pools.
    assert_eq!(GRAPH.pools.len(), 1);
}
