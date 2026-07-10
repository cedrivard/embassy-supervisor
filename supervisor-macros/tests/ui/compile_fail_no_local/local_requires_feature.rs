//! Without the (non-default) `local-resources` feature the `local` kind is
//! rejected: it is the one graph form that makes `supervisor_graph!` emit an
//! `unsafe impl Sync` into the consumer's crate, so it is strictly opt-in.

use std::cell::Cell;
use std::rc::Rc;

use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode, _b: &mut Rc<Cell<u32>>) {}

supervisor_graph! {
    node N = Terminate, deps: [], task: worker,
        resources: [BLOB: local Rc<Cell<u32>>];
}

fn main() {}
