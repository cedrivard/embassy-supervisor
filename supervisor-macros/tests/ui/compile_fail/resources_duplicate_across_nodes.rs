
use embassy_supervisor::{TaskNode, supervisor_graph};

struct Led;

async fn worker(_node: &'static TaskNode, _led: &mut Led) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: worker, resources: [LED: Led];
    node B = Terminate, deps: [], task: worker, resources: [LED: Led];
}

fn main() {}
