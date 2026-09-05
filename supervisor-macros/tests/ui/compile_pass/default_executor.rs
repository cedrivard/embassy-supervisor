
use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task(pool_size = 3)]
async fn spawned(_node: &'static TaskNode) {}
async fn worker(_node: &'static TaskNode) {}

// `default executor THREAD;` routes every node and pool that could have
// written `executor: THREAD` itself: `task:` shells and `spawn:` fns. B keeps
// its own tier; PARKED has no task source and a verbatim spawn closure picks
// its own spawner, so both stay on the supervisor's executor.
supervisor_graph! {
    default executor THREAD;
    executor HIGH;
    node A = Terminate, deps: [], task: worker;
    node B = Terminate, deps: [A], executor: HIGH, spawn: spawned;
    node PARKED = Pause, deps: [];
    node CLOSURE = Terminate, deps: [], spawn: |_s| Ok(());
    pool P = [Terminate, OnDemand], deps: [A],
        task: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    // Both slot statics exist and start unfilled.
    assert!(THREAD.get().is_none());
    assert!(HIGH.get().is_none());
    // Executor declarations occupy no graph slot: A, B, PARKED, CLOSURE, P0, P1.
    assert_eq!(GRAPH.nodes.len(), 6);
    assert_eq!(GRAPH.pools.len(), 1);
}
