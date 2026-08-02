//! `supervisor_fragment!` + `compose_graph!`: relays forward items into one
//! `supervisor_graph!` expansion; cross-fragment deps (both directions) and
//! compose-site items resolve together; order is dependency-first.

use embassy_supervisor::{TaskNode, compose_graph, supervisor_fragment};

pub async fn net_worker(_node: &'static TaskNode) {}
pub async fn http_worker(_node: &'static TaskNode) {}
async fn app_worker(_node: &'static TaskNode) {}

supervisor_fragment! {
    name: NET_FRAG;
    executor HIGH;
    node NET = Terminate, deps: [], task: $crate::net_worker;
}

supervisor_fragment! {
    name: HTTP_FRAG;
    node HTTP = Terminate, deps: [NET], task: $crate::http_worker;
}

compose_graph! {
    fragments: [NET_FRAG, HTTP_FRAG],
    graph: {
        node APP = Terminate, deps: [HTTP, NET], task: app_worker;
    }
}

fn main() {
    assert_eq!(GRAPH.order, [0, 1, 2]);
    assert_eq!(NET.name, "net");
    assert_eq!(HTTP.name, "http");
    assert_eq!(APP.name, "app");
    // The fragment's executor slot exists at the compose site.
    assert!(HIGH.get().is_none());
}
