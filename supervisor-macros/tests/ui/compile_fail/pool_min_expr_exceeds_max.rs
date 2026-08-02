//! Const-expr `min:`/`max:` are validated by const asserts (parse-time spans
//! only exist for the literal path): a floor above the ceiling fails
//! const-eval.

use embassy_supervisor::{TaskNode, supervisor_graph};

const FLOOR: usize = 3;

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    pool CRUNCH = [Terminate, OnDemand], deps: [],
        task: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: FLOOR, max: 2;
}

fn main() {}
