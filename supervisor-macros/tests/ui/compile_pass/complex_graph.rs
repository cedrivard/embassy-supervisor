
use embassy_supervisor::{DeferredShrink, TaskNode, supervisor_graph};

#[embassy_executor::task]
async fn wifi_task(_node: &'static TaskNode) {}
#[embassy_executor::task]
async fn clock_task(_node: &'static TaskNode) {}
#[embassy_executor::task]
async fn http_task(_node: &'static TaskNode) {}
#[embassy_executor::task(pool_size = 2)]
async fn worker_task(_node: &'static TaskNode) {}

supervisor_graph! {
    node WIFI = Terminate, deps: [], spawn: wifi_task;
    #[cfg(all())]
    node CLOCK = Terminate, deps: [WIFI], spawn: clock_task;
    #[cfg(all())]
    node HTTP = Terminate, deps: [WIFI], spawn: http_task;
    #[cfg(all())]
    pool WORKERS = [Terminate, OnDemand], deps: [WIFI],
        spawn: worker_task,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(2)),
        min: 1, max: 2;
    #[cfg(all())]
    node SENSOR = Pause, deps: [#[cfg(all())] CLOCK];
    #[cfg(all())]
    node BUTTON = Pause, deps: [];
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 7);
    assert!(GRAPH.nodes.iter().all(|n| n.is_some()));

    assert_eq!(GRAPH.deps_of(0).len(), 0);
    assert_eq!(GRAPH.deps_of(1), [0u8].as_slice());
    assert_eq!(GRAPH.deps_of(2), [0u8].as_slice());
    assert_eq!(GRAPH.deps_of(3), [0u8].as_slice());
    assert_eq!(GRAPH.deps_of(4), [0u8].as_slice());
    assert_eq!(GRAPH.deps_of(5), [1u8].as_slice()); 
    assert_eq!(GRAPH.deps_of(6).len(), 0);
    assert_eq!(GRAPH.pools.len(), 1);

    for (pos, n) in GRAPH.order().enumerate() {
        for &d in GRAPH.deps_of(n) {
            let dep_pos = GRAPH.order().position(|x| x == d).unwrap();
            assert!(dep_pos < pos);
        }
    }
}
