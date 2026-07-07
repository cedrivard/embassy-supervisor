//! `resources:` requires `task:` — a hand-written `spawn:` task fn owns its
//! argument handling, so there is no generated shell to take/restore the slots.

use embassy_supervisor::{TaskNode, supervisor_graph};

struct FakeLed;

#[embassy_executor::task]
async fn blink_task(_node: &'static TaskNode) {}

supervisor_graph! {
    node BLINK = Terminate, deps: [], spawn: blink_task,
        resources: [LED: FakeLed];
}

fn main() {}
