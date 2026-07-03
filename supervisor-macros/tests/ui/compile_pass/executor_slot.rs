//! The `executor NAME;` item declares a runtime-filled `SpawnerSlot`; a node or
//! a pool carrying `executor: NAME` routes its generated spawn glue through
//! that slot (a `SendSpawner`, so InterruptExecutor tiers / core1 are reachable
//! from the graph). Here the glue is only generated, not run — no executor
//! exists — so the test asserts the emitted items and the unfilled-slot state.

use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task(pool_size = 3)]
async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    executor HIGH;
    node A = Terminate, deps: [];
    node B = Terminate, deps: [A], executor: HIGH, spawn: worker;
    pool P = [Terminate, OnDemand], deps: [A], executor: HIGH,
        spawn: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    // The slot static exists and starts unfilled (spawning B or a P member now
    // would fail loudly with SpawnError::Busy instead of silently doing nothing).
    assert!(HIGH.get().is_none());
    // The executor declaration occupies no graph slot: A, B, P0, P1.
    assert_eq!(GRAPH.nodes.len(), 4);
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
    assert_eq!(GRAPH.pools.len(), 1);
    assert!(B.mode == embassy_supervisor::Mode::Terminate);
}
