//! A `node`/`pool` may depend on a `pool` by name; the dep resolves to the pool's
//! floor member (member 0, the `min`-kept one), i.e. "start after the pool is up".
//! Individual members are not separately name-addressable.

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
    // Slots in declaration order: A(0), POOLX0(1, floor), POOLX1(2), AFTER(3).
    assert_eq!(GRAPH.nodes.len(), 4);
    assert_eq!(GRAPH.pools.len(), 1);
    // The pool floor (slot 1) depends on A (slot 0).
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
    // AFTER (slot 3) depends on the POOL — resolved to its floor member, slot 1.
    assert_eq!(
        GRAPH.deps[3],
        [1u8].as_slice(),
        "a dep on a pool name resolves to the pool's floor member slot"
    );
}
