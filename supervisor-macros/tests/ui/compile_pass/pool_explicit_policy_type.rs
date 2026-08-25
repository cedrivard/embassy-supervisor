
use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task(pool_size = 2)]
async fn worker(_node: &'static TaskNode) {}

// A factory (not a `Ty::new(..)` constructor). Without the explicit type the macro would
// reject this value, since `policy_type` can't strip a type out of `make_policy()`.
const fn make_policy() -> DeferredShrink {
    DeferredShrink::new(embassy_time::Duration::from_secs(1))
}

supervisor_graph! {
    node A = Terminate, deps: [];
    pool P = [Terminate, OnDemand], deps: [A],
        spawn: worker,
        policy: DeferredShrink = make_policy(),
        min: 1, max: 2;
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 3);
    assert_eq!(GRAPH.pools.len(), 1);
    assert_eq!(GRAPH.deps_of(0).len(), 0);
    assert_eq!(GRAPH.deps_of(1), [0u8].as_slice());
    assert_eq!(GRAPH.deps_of(2), [0u8].as_slice());
}
