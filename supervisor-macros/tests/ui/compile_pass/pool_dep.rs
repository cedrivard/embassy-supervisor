
use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task(pool_size = 4)]
async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [];
    pool POOLX = [Terminate, OnDemand], deps: [A],
        spawn: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
    // Depends on the POOL name — resolves to POOLX's floor member (slot 1).
    node AFTER = Terminate, deps: [POOLX], spawn: worker;
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 4);
    assert_eq!(GRAPH.pools.len(), 1);
    assert_eq!(GRAPH.deps_of(1), [0u8].as_slice());
    assert_eq!(
        GRAPH.deps_of(3),
        [1u8].as_slice(),
        "a dep on a pool name resolves to the pool's floor member slot"
    );
}
