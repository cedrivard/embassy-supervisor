// `restart_budget: MAX in WINDOW_MS` lands on the node as `(max, window)`.
// `all()` keeps a gated clause, `any()` strips it.
use embassy_supervisor::{TaskNode, supervisor_graph};
use embassy_time::Duration;

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node NET = Terminate, deps: [], task: worker, restart_budget: 3 in 10000;
    node HTTP = Terminate, deps: [NET], task: worker, restart_budget: 65535 in 1, slot_timeout: 250;
    node GATED = Terminate, deps: [], task: worker, #[cfg(all())] restart_budget: 2 in 500;
    node STRIPPED = Terminate, deps: [], task: worker, #[cfg(any())] restart_budget: 2 in 500;
    node PLAIN = Terminate, deps: [], task: worker;
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 5);
    assert_eq!(NET.restart_budget(), Some((3, Duration::from_millis(10_000))));
    assert_eq!(HTTP.restart_budget(), Some((u16::MAX, Duration::from_millis(1))));
    assert_eq!(HTTP.slot_timeout(), Duration::from_millis(250));
    assert_eq!(GATED.restart_budget(), Some((2, Duration::from_millis(500))));
    assert_eq!(STRIPPED.restart_budget(), None);
    assert_eq!(PLAIN.restart_budget(), None);
}
