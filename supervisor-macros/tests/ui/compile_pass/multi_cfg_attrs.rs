//! Two `#[cfg(...)]` attributes on one node are combined into a single `all(..)`
//! presence predicate (`cfg_predicate`'s len > 1 branch).

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    #[cfg(all())]
    #[cfg(all())]
    node B = Terminate, deps: [A];
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 2);
    // Both predicates are true, so B is present.
    assert!(GRAPH.nodes[0].is_some());
    assert!(GRAPH.nodes[1].is_some());
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
}
