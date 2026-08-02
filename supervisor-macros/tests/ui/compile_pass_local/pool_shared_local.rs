//! `shared local` resources on a `pool`: the one pool kind combining a `!Send`
//! payload with fan-out (the `embassy_net::Stack` shape — a `Copy` handle that
//! is not `Send`). The pool rides the pool-wide shared-slot path exactly like
//! nodes: ONE static (declared here by a node AND the pool), non-destructive
//! `get()` per spawn, no restore. Take-kind `local` on pools stays rejected
//! (see compile_fail_local/local_on_pool.rs).

use embassy_supervisor::{TaskNode, supervisor_graph};

/// A `Copy` fan-out handle that is also `!Send` (raw pointer) — the
/// `embassy_net::Stack` shape, needing `shared local`.
type LocalHandle = (u32, *const ());

async fn provider(_node: &'static TaskNode) {}
async fn consumer(_node: &'static TaskNode, _s: LocalHandle) {}
async fn fan_worker(_node: &'static TaskNode, _s: LocalHandle) {}

supervisor_graph! {
    node PROVIDER = Terminate, deps: [], task: provider;

    // The pool declares the SAME `shared local` slot as the node: one static,
    // union cfg, every member copies the same handle out.
    pool FANS = [Terminate, OnDemand], deps: [PROVIDER],
        task: fan_worker,
        resources: [S: shared local LocalHandle],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
    node USER = Terminate, deps: [PROVIDER],
        task: consumer,
        resources: [S: shared local LocalHandle];
}

fn main() {
    // One pool-wide slot with the ResourceSlot protocol: `get()` copies
    // without emptying, so any number of members/consumers fan out.
    S.provide((5, core::ptr::null()));
    assert_eq!(S.get().expect("shared local get").0, 5);
    assert!(S.get().is_some(), "shared local slot stays filled");

    // Pool + two nodes: members + PROVIDER + USER.
    assert_eq!(GRAPH.nodes.len(), 4);
}
