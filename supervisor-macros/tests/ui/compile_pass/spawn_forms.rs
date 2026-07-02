//! All `spawn:` forms compile: bare path, partial call (node injected first), and a
//! verbatim closure for a node; a pool `spawn:` (path and partial-call). `main` never
//! spawns anything (no executor is running) — this only checks the generated code
//! type-checks and links.

use embassy_executor::SpawnError;
use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn plain(_node: &'static TaskNode) {}

#[embassy_executor::task(pool_size = 8)]
async fn with_arg(_node: &'static TaskNode, _extra: u32) {}

fn seven() -> u32 {
    7
}

supervisor_graph! {
    node P = Terminate, deps: [], spawn: plain;                    // bare path
    node Q = Terminate, deps: [P], spawn: with_arg(seven());      // partial call -> with_arg(&Q, 7)
    node R = Terminate, deps: [P], spawn: |_s| Ok::<(), SpawnError>(()); // verbatim closure
    node PARKED = Pause, deps: [P];                                // no spawn
    pool W = [Terminate, OnDemand], deps: [P],
        spawn: with_arg(seven()),                                 // per-member: with_arg(&W[i], 7)
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    // P, Q, R, PARKED, W0, W1
    assert_eq!(GRAPH.nodes.len(), 6);
    assert_eq!(GRAPH.pools.len(), 1);
}
