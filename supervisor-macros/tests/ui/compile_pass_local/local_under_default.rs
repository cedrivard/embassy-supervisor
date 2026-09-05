
use std::cell::Cell;
use std::rc::Rc;

use embassy_supervisor::{TaskNode, supervisor_graph};

async fn provider(_node: &'static TaskNode) {}
async fn worker(_node: &'static TaskNode, _b: Rc<Cell<u32>>) {}

// Neither node writes `executor:`; both inherit the graph default, so the
// `local` slot's declarations agree on a tier and the check passes.
supervisor_graph! {
    default executor THREAD;
    node P = Terminate, deps: [], task: provider, provides: [BLOB];
    node N = Terminate, deps: [P], task: worker,
        resources: [BLOB: local consume Rc<Cell<u32>>];
}

fn main() {
    assert!(THREAD.get().is_none());
    assert_eq!(GRAPH.nodes.len(), 2);
}
