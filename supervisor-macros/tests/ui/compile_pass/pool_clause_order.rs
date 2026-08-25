
use embassy_supervisor::{DeferredShrink, supervisor_graph};

async fn worker() {}

supervisor_graph! {
    node A = Terminate, deps: [];

    pool P = [Terminate, OnDemand],
        max: 2,
        slot_timeout: 500,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        task: worker,
        cancel,
        min: 1,
        deps: [A];
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 3);
    assert_eq!(GRAPH.pools.len(), 1);
    assert_eq!(GRAPH.deps_of(0).len(), 0);
    assert_eq!(GRAPH.deps_of(1), [0u8].as_slice());
    assert_eq!(GRAPH.deps_of(2), [0u8].as_slice());
    assert_eq!(P_MIN, 1);
    assert_eq!(P_MAX, 2);
    assert_eq!(P_MEMBERS, P.len());
    assert_eq!(P[0].slot_timeout(), embassy_time::Duration::from_millis(500));
    assert_eq!(P[1].slot_timeout(), embassy_time::Duration::from_millis(500));
}
