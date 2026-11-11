
use embassy_supervisor::dataflow;

#[dataflow(read(is_running))]
fn f(node: &'static embassy_supervisor::TaskNode) -> bool {
    node.is_running()
}

fn main() {}
