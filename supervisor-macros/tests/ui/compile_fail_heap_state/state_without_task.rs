
use embassy_supervisor::{TaskNode, supervisor_graph};

struct Big([u8; 1024]);

#[embassy_executor::task]
async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node CRUNCH = Terminate, deps: [], spawn: worker, state: Big = Big([0; 1024]);
}

fn main() {}
