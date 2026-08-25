
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
    node P = Terminate, deps: [], spawn: plain;                    
    node Q = Terminate, deps: [P], spawn: with_arg(seven());      
    node R = Terminate, deps: [P], spawn: |_s| Ok::<(), SpawnError>(()); 
    node PARKED = Pause, deps: [P];                                
    pool W = [Terminate, OnDemand], deps: [P],
        spawn: with_arg(seven()),                                 
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 6);
    assert_eq!(GRAPH.pools.len(), 1);
}
