//! A repeated kind marker on one `resources:` entry is a declaration bug.

use embassy_supervisor::{TaskNode, supervisor_graph};

struct FakeLed;

async fn worker(_node: &'static TaskNode, _led: FakeLed) {}

supervisor_graph! {
    node BLINK = Terminate, deps: [], task: worker,
        resources: [LED: consume consume FakeLed];
}

fn main() {}
