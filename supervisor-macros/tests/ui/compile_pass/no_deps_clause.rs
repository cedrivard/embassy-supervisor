
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

async fn worker(node: &'static TaskNode) {
    node.ack_dropped();
}

supervisor_graph! {
    node A = Terminate, task: worker;
    node B = Terminate, task: worker, disabled;
    node C = Terminate, task: worker, deps: [A];
}

fn main() {
    let _sup = Supervisor::new(&GRAPH);
    assert_eq!(GRAPH.deps_of(0), &[] as &[u8]);
    assert_eq!(GRAPH.deps_of(2), &[0]);
}
