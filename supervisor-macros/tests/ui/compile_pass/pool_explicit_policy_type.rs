//! The optional `policy: <Type> = <expr>` form: state the policy type explicitly so
//! the value can be any (const-evaluable) expression of that type — here a `const fn`
//! factory call, which the default `Ty::new(..)` derivation (`policy_type`) cannot
//! handle (a single-segment call has no type to strip). Proves the annotated type is
//! used verbatim for the emitted `ElasticPool<P>` and the value shape is unconstrained.
//!
//! The value must still be const-evaluable, since it initializes the pool `static` —
//! hence a `const fn` rather than a plain fn.

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
    // A(0) + two pool members P0(1), P1(2).
    assert_eq!(GRAPH.nodes.len(), 3);
    assert_eq!(GRAPH.pools.len(), 1);
    assert_eq!(GRAPH.deps[0].len(), 0);
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
    assert_eq!(GRAPH.deps[2], [0u8].as_slice());
}
