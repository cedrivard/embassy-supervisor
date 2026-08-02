//! A name collision across fragments hits the ordinary duplicate-name pass,
//! attributed to the owning fragment.

use embassy_supervisor::{TaskNode, compose_graph, supervisor_fragment};

async fn worker(_node: &'static TaskNode) {}

supervisor_fragment! {
    name: FRAG_A;
    node NET = Terminate, deps: [], task: $crate::worker;
}

supervisor_fragment! {
    name: FRAG_B;
    node NET = Terminate, deps: [], task: $crate::worker;
}

compose_graph! {
    fragments: [FRAG_A, FRAG_B],
    graph: {}
}

fn main() {}
