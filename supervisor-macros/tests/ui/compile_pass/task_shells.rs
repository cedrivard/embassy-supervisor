//! `task:` — the macro stamps a concrete `#[embassy_executor::task]` shell per
//! declaration for a plain (possibly generic) async worker fn, which embassy's
//! own attribute cannot accept ("task functions must not be generic"). Covers:
//! turbofish and inferred instantiations, a `Pause` node, `pool_size:`, a whole
//! pool driven by one worker, and a `#[cfg]`-gated `task:` node.

use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

trait Beat {
    const HZ: u32;
}
struct Fast;
struct Slow;
impl Beat for Fast {
    const HZ: u32 = 100;
}
impl Beat for Slow {
    const HZ: u32 = 1;
}

/// A generic worker: NOT a `#[embassy_executor::task]` — the macro generates one
/// shell per graph declaration below.
async fn ticker<B: Beat>(_node: &'static TaskNode, _extra: u32) {
    let _ = B::HZ;
}

/// Worker with only the node param (bare-path `task:` form).
async fn plain_worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node FAST = Terminate, deps: [], task: ticker::<Fast>(7);
    node SLOW = Pause, deps: [FAST], task: ticker::<Slow>(9), pool_size: 2;
    node BARE = Terminate, deps: [], task: plain_worker;
    #[cfg(any())] // always false: the gated node keeps a None slot
    node GATED = Terminate, deps: [], task: ticker::<Fast>(0);
    pool CRUNCH = [Terminate, OnDemand], deps: [FAST],
        task: ticker::<Slow>(1),
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    // Slots: FAST(0), SLOW(1), BARE(2), GATED(3, cfg'd out => None), CRUNCH0(4), CRUNCH1(5).
    assert_eq!(GRAPH.nodes.len(), 6);
    assert!(GRAPH.nodes[3].is_none(), "cfg'd-out task: node keeps a None slot");
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
    assert_eq!(GRAPH.pools.len(), 1);
}
