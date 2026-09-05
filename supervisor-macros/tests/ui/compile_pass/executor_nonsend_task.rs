
use std::cell::Cell;
use std::rc::Rc;

use embassy_supervisor::{TaskNode, supervisor_graph};

// The worker's future is `!Send`: an `Rc` lives across an await. It still
// routes through `executor: HIGH`, because `SendSpawner::spawn`'s bound falls
// on the spawn arguments (the shell's, always `Send`), not on the future,
// which embassy builds on the target executor at first poll.
async fn worker(node: &'static TaskNode) {
    let held = Rc::new(Cell::new(0u32));
    core::future::ready(()).await;
    held.set(node.is_running() as u32);
    node.ack_dropped();
}

supervisor_graph! {
    executor HIGH;
    node A = Terminate, deps: [], executor: HIGH, task: worker;
}

fn main() {
    assert!(HIGH.get().is_none());
    assert_eq!(GRAPH.nodes.len(), 1);
}
