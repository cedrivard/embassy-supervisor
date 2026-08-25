
use embassy_supervisor::dataflow;

pub static OUT: u32 = 0;

#[dataflow(read(is_running))]
fn f(node: &'static embassy_supervisor::TaskNode) -> bool {
    node.writer(&crate::OUT);
    node.is_running()
}

fn main() {}
