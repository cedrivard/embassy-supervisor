//! An unresolved cross-fragment dep fails at the compose site, attributed to
//! the fragment that declared it.

use embassy_supervisor::{TaskNode, compose_graph, supervisor_fragment};

async fn worker(_node: &'static TaskNode) {}

supervisor_fragment! {
    name: LONELY_FRAG;
    node HTTP = Terminate, deps: [NET], task: $crate::worker;
}

compose_graph! {
    fragments: [LONELY_FRAG],
    graph: {}
}

fn main() {}
