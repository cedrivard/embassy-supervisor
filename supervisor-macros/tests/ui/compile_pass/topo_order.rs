//! `GRAPH.order` is a valid topological order of a diamond graph.
//!
//! Parked nodes (no `spawn:`) keep this free of embassy-executor task machinery —
//! we're testing the graph structure, not spawning.

use embassy_supervisor::supervisor_graph;

// Diamond: D depends on B and C, both depend on A.
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

    // Every node appears in GRAPH.order after all of its dependencies.
    for (pos, &n) in GRAPH.order.iter().enumerate() {
        for &d in GRAPH.deps[n as usize] {
            let dep_pos = GRAPH.order.iter().position(|&x| x == d).unwrap();
            assert!(dep_pos < pos, "dep {} must precede node {}", d, n);
        }
    }
}
