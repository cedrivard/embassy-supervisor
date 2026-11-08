// User items may be named `NODES`, `DEPS`, `GRAPH_REF` or `spawn_<pool>`: the
// expansion's own helpers all carry a `__SV_`/`__sv_` prefix.
use embassy_supervisor::{TaskNode, supervisor_graph};

static NODES: [u8; 2] = [0, 1];
const DEPS: u8 = 2;
static GRAPH_REF: u8 = 3;
fn spawn_net() -> u8 {
    4
}

async fn member(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [];
    pool NET = [Terminate], deps: [A], task: member,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 1;
}

fn main() {
    assert_eq!(NODES[1] + DEPS + GRAPH_REF + spawn_net(), 10);
    assert_eq!(GRAPH.nodes.len(), 2);
}
