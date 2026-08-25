
use std::cell::Cell;
use std::rc::Rc;

use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode, _b: &mut Rc<Cell<u32>>) {}

supervisor_graph! {
    node N = Terminate, deps: [], task: worker,
        resources: [BLOB: local Rc<Cell<u32>>];
}

fn main() {}
