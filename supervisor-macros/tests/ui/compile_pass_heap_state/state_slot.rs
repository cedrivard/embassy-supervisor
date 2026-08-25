
use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

pub struct Scratch {
    pub buf: [u8; 256],
}

async fn crunch_worker(_node: &'static TaskNode, _state: &mut Scratch) {}
async fn pool_worker(_node: &'static TaskNode, _port: u16, _state: &mut Scratch) {}

supervisor_graph! {
    node CRUNCH = Terminate, deps: [], task: crunch_worker,
        state: Scratch = Scratch { buf: [0; 256] };
    pool WORKERS = [Terminate, OnDemand], deps: [CRUNCH],
        task: pool_worker,
        resources: [PORT: shared u16],
        state: Scratch = Scratch { buf: [1; 256] },
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
}

fn main() {
    assert!(GRAPH.order().eq([0, 1, 2]));
    assert_eq!(WORKERS_MEMBERS, 2);
}
