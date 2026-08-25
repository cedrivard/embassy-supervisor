
use embassy_supervisor::{TaskNode, dataflow};

pub static X: u32 = 0;

#[dataflow]
fn f(node: &'static TaskNode) {
    let hidden = &X;
    node.reader(hidden);
}

fn main() {}
