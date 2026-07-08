//! `task:` and `spawn:` are mutually exclusive: one names a hand-written
//! `#[embassy_executor::task]` fn, the other asks the macro to generate one.

use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn real_task(_node: &'static TaskNode) {}

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], spawn: real_task, task: worker;
}

fn main() {}
