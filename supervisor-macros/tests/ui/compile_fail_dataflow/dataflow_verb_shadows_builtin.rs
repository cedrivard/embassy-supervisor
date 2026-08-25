
use embassy_supervisor::dataflow;

pub static OUT: u32 = 0;

#[dataflow(read(put))]
fn f(node: &'static embassy_supervisor::TaskNode) {
    node.writer(&crate::OUT);
}

fn main() {}
