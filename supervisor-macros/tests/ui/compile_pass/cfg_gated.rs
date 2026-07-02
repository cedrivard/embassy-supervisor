//! `#[cfg]`-ed-out nodes become `None` slots, and cfg-gated deps drop out.
//!
//! `cfg(any())` is always false and `cfg(all())` always true — a feature-free way to
//! exercise the macro's cfg handling deterministically.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    #[cfg(any())]
    node GONE = Terminate, deps: [A];
    #[cfg(all())]
    node HERE = Terminate, deps: [A];
    node D = Terminate, deps: [A, #[cfg(any())] GONE, #[cfg(all())] HERE];
}

fn main() {
    // 4 declared slots regardless of cfg (GONE keeps its slot as `None`).
    assert_eq!(GRAPH.nodes.len(), 4);
    assert!(GRAPH.nodes[0].is_some()); // A
    assert!(GRAPH.nodes[1].is_none()); // GONE, cfg-ed out
    assert!(GRAPH.nodes[2].is_some()); // HERE
    assert!(GRAPH.nodes[3].is_some()); // D

    // D's cfg-gated dep on GONE is dropped; the dep on HERE is kept.
    assert_eq!(GRAPH.deps[3], [0u8, 2u8].as_slice()); // A(0), HERE(2)
}
