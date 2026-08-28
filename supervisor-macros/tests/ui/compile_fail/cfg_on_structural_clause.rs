// `#[cfg(...)]` may only gate the value-level clauses — a structural clause
// like `task:` changes the generated items themselves, so the gate belongs on
// the whole node.
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node N = Terminate, deps: [], #[cfg(feature = "x")] task: worker;
}

fn main() {}
