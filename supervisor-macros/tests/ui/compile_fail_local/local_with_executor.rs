//! `local` resources cannot combine with `executor:` — a local slot carries
//! `!Send` values, and an `executor:`-routed node spawns through a `SendSpawner`
//! whose `spawn` requires a `Send` future. The macro rejects the combination
//! with the reason, instead of rustc's opaque `F: Send` bound failure.

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
