
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    node B = Terminate, deps: [A];
    node C = Terminate, deps: [A];
    node D = Terminate, deps: [B, C];
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 4);
    assert_eq!(GRAPH.nodes.len(), 4);
    assert!(GRAPH.nodes.iter().all(|n| n.is_some()));

    for (pos, n) in GRAPH.order().enumerate() {
        for &d in GRAPH.deps_of(n) {
            let dep_pos = GRAPH.order().position(|x| x == d).unwrap();
            assert!(dep_pos < pos, "dep {} must precede node {}", d, n);
        }
    }
}
