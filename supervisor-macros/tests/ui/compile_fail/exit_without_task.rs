//! `exit:` requires `task:` — only the generated shell can capture the worker's
//! return value; a hand-written `spawn:` task fn can provide() into a slot itself.

use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn probe_task(_node: &'static TaskNode) {}

supervisor_graph! {
    node PROBE = Terminate, deps: [], spawn: probe_task, exit: u32;
}

fn main() {}
