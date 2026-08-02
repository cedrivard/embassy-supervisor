//! The `state:` clause requires the `heap-state` feature (it emits the
//! consumer-crate fallible-boxing helper and an alloc dependency).

use embassy_supervisor::{TaskNode, supervisor_graph};

struct Big([u8; 1024]);

async fn worker(_node: &'static TaskNode, _s: &mut Big) {}

supervisor_graph! {
    node CRUNCH = Terminate, deps: [], task: worker, state: Big = Big([0; 1024]);
}

fn main() {}
