//! Trailing commas are tolerated in both node/pool dependency lists
//! (`parse_dep_list`) and the pool mode list (`Punctuated::parse_terminated`).

use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task(pool_size = 2)]
async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [];
    node B = Terminate, deps: [A,];
    node C = Terminate, deps: [A, B,];
    pool P = [Terminate, OnDemand,], deps: [A,],
        spawn: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    // A,B,C + two pool members P0,P1.
    assert_eq!(GRAPH.nodes.len(), 5);
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
    assert_eq!(GRAPH.deps[2], [0u8, 1u8].as_slice());
    assert_eq!(GRAPH.pools.len(), 1);
}
