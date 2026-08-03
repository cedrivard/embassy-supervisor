//! `cancel` requires `task:` — it rewrites how the generated shell drives the
//! worker; a hand-written `spawn:` task fn can call `node.run_cancellable(..)`
//! itself.

use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn probe_task(_node: &'static TaskNode) {}

supervisor_graph! {
    node PROBE = Terminate, deps: [], spawn: probe_task, cancel;
}

fn main() {}
