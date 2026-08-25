
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    #[cfg(all())]
    #[cfg(all())]
    node B = Terminate, deps: [A];
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 2);
    assert!(GRAPH.nodes[0].is_some());
    assert!(GRAPH.nodes[1].is_some());
    assert_eq!(GRAPH.deps_of(1), [0u8].as_slice());
}
