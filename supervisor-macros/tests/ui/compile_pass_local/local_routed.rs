
use std::cell::Cell;
use std::rc::Rc;

use embassy_supervisor::{TaskNode, supervisor_graph};

type LocalHandle = (u32, *const ());

async fn provider(_node: &'static TaskNode) {}
async fn worker(_node: &'static TaskNode, _b: Rc<Cell<u32>>) {}
async fn fan_worker(_node: &'static TaskNode, _s: LocalHandle) {}
async fn consumer(_node: &'static TaskNode, _s: LocalHandle) {}

// `local` routes through an executor slot as long as every declaration of
// the slot, provider included, resolves to that same executor: the value
// never leaves the tier that built it.
supervisor_graph! {
    executor HIGH;
    node P = Terminate, deps: [], executor: HIGH, task: provider, provides: [BLOB];
    node N = Terminate, deps: [P], executor: HIGH, task: worker,
        resources: [BLOB: local consume Rc<Cell<u32>>];

    pool FANS = [Terminate, OnDemand], deps: [P], executor: HIGH,
        task: fan_worker,
        resources: [S: shared local LocalHandle],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
    node USER = Terminate, deps: [P], executor: HIGH,
        task: consumer,
        resources: [S: shared local LocalHandle];
}

fn main() {
    assert!(HIGH.get().is_none());
    S.provide((5, core::ptr::null()));
    assert_eq!(S.get().expect("shared local get").0, 5);
    // P, N, FANS x2, USER.
    assert_eq!(GRAPH.nodes.len(), 5);
}
