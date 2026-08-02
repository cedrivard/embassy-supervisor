//! `compose_graph! { name: X, fragments: [...], graph: {...} }` forwards the
//! name into the final expansion — a composed graph can be a named sub-graph
//! next to an unnamed primary.

use embassy_supervisor::{TaskNode, compose_graph, supervisor_fragment, supervisor_graph};

pub async fn net_worker(_node: &'static TaskNode) {}
async fn app_worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node MAIN = Terminate, deps: [], task: app_worker;
}

supervisor_fragment! {
    name: NET_FRAG;
    node NET = Terminate, deps: [], task: $crate::net_worker;
}

compose_graph! {
    name: NET_GRAPH,
    fragments: [NET_FRAG],
    graph: {
        node APP = Terminate, deps: [NET], task: app_worker;
    }
}

fn main() {
    assert_eq!(GRAPH.order.len(), 1);
    assert_eq!(NET_GRAPH.order, [0, 1]);
    assert_eq!(NET.name, "net");
}
