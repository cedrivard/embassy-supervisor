
use embassy_supervisor::dataflow;

pub static OUT: u32 = 0;

#[dataflow(beat = crate::OUT)]
fn f(node: &'static embassy_supervisor::TaskNode) {
    node.writer(&crate::OUT);
}

fn main() {}
