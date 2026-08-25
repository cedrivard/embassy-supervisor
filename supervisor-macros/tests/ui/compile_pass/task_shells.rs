
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

async fn ticker<B: Beat>(_node: &'static TaskNode, _extra: u32) {
    let _ = B::HZ;
}

/// Worker with only the node param (bare-path `task:` form).
async fn plain_worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node FAST = Terminate, deps: [], task: ticker::<Fast>(7);
    node SLOW = Pause, deps: [FAST], task: ticker::<Slow>(9), pool_size: 2;
    node BARE = Terminate, deps: [], task: plain_worker;
    #[cfg(any())] 
    node GATED = Terminate, deps: [], task: ticker::<Fast>(0);
    pool CRUNCH = [Terminate, OnDemand], deps: [FAST],
        task: ticker::<Slow>(1),
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 6);
    assert!(GRAPH.nodes[3].is_none(), "cfg'd-out task: node keeps a None slot");
    assert_eq!(GRAPH.deps_of(1), [0u8].as_slice());
    assert_eq!(GRAPH.pools.len(), 1);
}
