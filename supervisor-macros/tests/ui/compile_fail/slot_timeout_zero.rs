//! `slot_timeout: 0` would make every gated spawn fail instantly.

use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node N = Terminate, deps: [], task: worker, slot_timeout: 0;
}

fn main() {}
