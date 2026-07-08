//! An elastic pool with a single mode (k = 1) generates one member slot. The
//! existing spawn_forms.rs only exercises k = 2, so this pins the k = 1 edge.

use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [];
    pool P = [Terminate], deps: [A],
        spawn: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 1;
}

fn main() {
    // A(0) + one pool member P0(1).
    assert_eq!(GRAPH.nodes.len(), 2);
    assert_eq!(GRAPH.pools.len(), 1);
    assert_eq!(GRAPH.deps[0].len(), 0);
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
    // The emitted structural constants: usable in const context (a `const` can't
    // read them off the `static P` array — constants cannot refer to statics).
    const BUDGET: usize = P_MAX + 1;
    assert_eq!(P_MIN, 1);
    assert_eq!(P_MAX, 1);
    assert_eq!(P_MEMBERS, P.len());
    assert_eq!(BUDGET, 2);
}
