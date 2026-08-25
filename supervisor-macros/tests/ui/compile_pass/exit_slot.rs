
use embassy_supervisor::{Aborted, TaskNode, supervisor_graph};

async fn score_worker(_node: &'static TaskNode) -> u32 {
    42
}

async fn serve_worker(node: &'static TaskNode) -> Result<u32, Aborted> {
    node.run_cancellable(core::future::pending::<u32>()).await
}

supervisor_graph! {
    node SCORE = Terminate, deps: [], task: score_worker, exit: u32;
    node SERVE = Terminate, deps: [SCORE], task: serve_worker, exit: Result<u32, Aborted>;
}

fn main() {
    assert!(SCORE_EXIT.take().is_none());
    assert!(SERVE_EXIT.take().is_none());
    assert!(GRAPH.order().eq([0, 1]));
}
