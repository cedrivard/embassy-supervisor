//! A linear chain A->B->C->D->E has exactly one valid topological order. Parked
//! nodes (no spawn) keep this free of executor machinery; asserts the exact GRAPH.order,
//! which topo_order.rs (diamond) only checks as a property.

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
    assert_eq!(GRAPH.deps[4], [3u8].as_slice());
    // Kahn's algorithm on an already-linear graph yields ascending indices.
    assert_eq!(GRAPH.order, [0u8, 1, 2, 3, 4]);
}
