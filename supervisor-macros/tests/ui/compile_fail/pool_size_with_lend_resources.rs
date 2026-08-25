
use embassy_supervisor::supervisor_graph;

async fn worker(_node: &'static embassy_supervisor::TaskNode, _r: &mut u32) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: worker, pool_size: 2,
        resources: [R: u32];
}

fn main() {}
