use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: worker;
}

supervisor_graph! {
    name: OFF_GRAPH;
    #[cfg(any())] supervisor priority: 3;
    node B = Terminate, deps: [], task: worker;
}

supervisor_graph! {
    name: ON_GRAPH;
    #[cfg(all())] supervisor priority: 3;
    node C = Terminate, deps: [], task: worker;
}

fn main() {
    assert_eq!(GRAPH.supervisor_priority, 255);
    assert_eq!(OFF_GRAPH.supervisor_priority, 255);
    assert_eq!(ON_GRAPH.supervisor_priority, 3);
}
