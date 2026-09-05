
use embassy_supervisor::{TaskNode, compose_graph, supervisor_fragment};

pub async fn net_worker(_node: &'static TaskNode) {}
async fn app_worker(_node: &'static TaskNode) {}

supervisor_fragment! {
    name: NET_FRAG;
    node NET = Terminate, deps: [], task: crate::net_worker;
}

// The compose site owns the default; the fragment's node inherits it without
// knowing the graph's executors.
compose_graph! {
    fragments: [NET_FRAG],
    graph: {
        default executor THREAD;
        node APP = Terminate, deps: [NET], task: app_worker;
    }
}

fn main() {
    assert!(THREAD.get().is_none());
    assert_eq!(NET.name(), "net");
    assert_eq!(APP.name(), "app");
    assert_eq!(GRAPH.nodes.len(), 2);
}
