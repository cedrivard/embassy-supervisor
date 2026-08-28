// Only `#[cfg(...)]` attributes may gate a clause — anything else has no
// defined meaning on a clause and is rejected.
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node N = Terminate, deps: [], task: worker, #[allow(dead_code)] slot_timeout: 100;
}

fn main() {}
