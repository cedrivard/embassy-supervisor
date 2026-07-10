//! `shared` and `consume` contradict each other: `consume` takes the single
//! value out for one owner, `shared` copies it out to any number of consumers.

use embassy_supervisor::{TaskNode, supervisor_graph};

#[derive(Clone, Copy)]
struct Handle;

async fn worker(_node: &'static TaskNode, _h: Handle) {}

supervisor_graph! {
    node N = Terminate, deps: [], task: worker,
        resources: [H: shared consume Handle];
}

fn main() {}
