
use embassy_supervisor::{TaskNode, supervisor_graph};

fn setup() -> Result<u32, ()> {
    Ok(42)
}
fn stack() -> u32 {
    7
}

#[embassy_executor::task]
async fn root_task(_node: &'static TaskNode, _setup: u32, _stack: u32) {}

supervisor_graph! {
    node ROOT = Terminate, deps: [],
        spawn: root_task(setup().expect("setup failed"), stack());
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 1);
    assert!(GRAPH.nodes[0].is_some());
    assert_eq!(GRAPH.deps_of(0).len(), 0);
    assert_eq!(GRAPH.order().next(), Some(0));
}
