//! `#[cfg(...)]` on individual entries of a pool's `deps:` list are filtered per-dep
//! at const-eval, exactly as for node deps (cfg_gated.rs covers the node case).

use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [];
    node B = Terminate, deps: [A];
    node C = Terminate, deps: [A];
    pool P = [Terminate], deps: [A, #[cfg(any())] B, #[cfg(all())] C],
        spawn: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 1;
}

fn main() {
    // A(0), B(1), C(2), pool member P0(3).
    assert_eq!(GRAPH.nodes.len(), 4);
    // B's cfg-gated dep drops; A and C remain: [A(0), C(2)].
    assert_eq!(GRAPH.deps[3], [0u8, 2u8].as_slice());
    assert_eq!(GRAPH.pools.len(), 1);
}
