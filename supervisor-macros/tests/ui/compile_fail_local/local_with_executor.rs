
use std::cell::Cell;
use std::rc::Rc;

use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode, _b: Rc<Cell<u32>>) {}

supervisor_graph! {
    executor HIGH;
    node N = Terminate, deps: [], executor: HIGH, task: worker,
        resources: [BLOB: local consume Rc<Cell<u32>>];
}

fn main() {}
