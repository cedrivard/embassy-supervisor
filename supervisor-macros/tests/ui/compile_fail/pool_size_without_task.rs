//! `pool_size:` sizes the generated shell's TaskPool, so it only makes sense
//! with `task:` — a `spawn:` task fn declares its own
//! `#[embassy_executor::task(pool_size = ...)]`.

use embassy_supervisor::{TaskNode, supervisor_graph};

#[embassy_executor::task(pool_size = 2)]
async fn real_task(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], spawn: real_task, pool_size: 2;
}

fn main() {}
