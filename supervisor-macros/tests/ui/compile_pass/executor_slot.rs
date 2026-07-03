//! The `executor NAME;` item declares a runtime-filled `SpawnerSlot`, and a node
//! carrying `executor: NAME` routes its generated spawn glue through that slot
//! (a `SendSpawner`, so InterruptExecutor tiers / core1 are reachable from the
//! graph). Here the glue is only generated, not run — no executor exists — so
//! the test asserts the emitted items and the unfilled-slot state.

use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    executor HIGH;
    node A = Terminate, deps: [];
    node B = Terminate, deps: [A], executor: HIGH, spawn: worker;
}

fn main() {
    // The slot static exists and starts unfilled (spawning B now would fail
    // loudly with SpawnError::Busy instead of silently doing nothing).
    assert!(HIGH.get().is_none());
    // The executor declaration occupies no graph slot.
    assert_eq!(GRAPH.nodes.len(), 2);
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
    assert!(B.mode == embassy_supervisor::Mode::Terminate);
}
