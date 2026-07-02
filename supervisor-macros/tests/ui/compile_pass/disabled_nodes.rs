//! `disabled` marks a node disabled-at-boot. Covers both forms in one graph: a
//! spawned node with `spawn: f, disabled`, and a parked node (no `spawn:`) with
//! just `disabled`. Both still occupy slots and participate in the topo order.

use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn base_task(_node: &'static TaskNode) {}

#[embassy_executor::task]
async fn ctrl_task(_node: &'static TaskNode) {}

supervisor_graph! {
    node BASE = Terminate, deps: [], spawn: base_task;
    node PARKED_OFF = Pause, deps: [BASE], disabled;              // disabled, no spawn
    node CTRL = Terminate, deps: [BASE], spawn: ctrl_task, disabled; // spawn: + disabled
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 3);
    assert!(GRAPH.nodes.iter().all(|n| n.is_some()));
    assert_eq!(GRAPH.deps[0].len(), 0);
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
    assert_eq!(GRAPH.deps[2], [0u8].as_slice());

    for (pos, &n) in GRAPH.order.iter().enumerate() {
        for &d in GRAPH.deps[n as usize] {
            let dep_pos = GRAPH.order.iter().position(|&x| x == d).unwrap();
            assert!(dep_pos < pos, "dep {} must precede node {}", d, n);
        }
    }
}
