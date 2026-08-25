
use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn base_task(_node: &'static TaskNode) {}

#[embassy_executor::task]
async fn ctrl_task(_node: &'static TaskNode) {}

supervisor_graph! {
    node BASE = Terminate, deps: [], spawn: base_task;
    node PARKED_OFF = Pause, deps: [BASE], disabled;              
    node CTRL = Terminate, deps: [BASE], spawn: ctrl_task, disabled; 
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 3);
    assert!(GRAPH.nodes.iter().all(|n| n.is_some()));
    assert_eq!(GRAPH.deps_of(0).len(), 0);
    assert_eq!(GRAPH.deps_of(1), [0u8].as_slice());
    assert_eq!(GRAPH.deps_of(2), [0u8].as_slice());

    for (pos, n) in GRAPH.order().enumerate() {
        for &d in GRAPH.deps_of(n) {
            let dep_pos = GRAPH.order().position(|x| x == d).unwrap();
            assert!(dep_pos < pos, "dep {} must precede node {}", d, n);
        }
    }
}
