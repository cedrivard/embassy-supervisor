
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    node B = Terminate, deps: [A];
    node C = Terminate, deps: [B];
    node D = Terminate, deps: [C];
    node E = Terminate, deps: [D];
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 5);
    assert_eq!(GRAPH.deps_of(4), [3u8].as_slice());
    assert!(GRAPH.order().eq([0u8, 1, 2, 3, 4]));
}
