//! End-to-end capstone: spawned nodes, a cfg-gated elastic pool (k=2), parked
//! Pause nodes, and a cfg-gated dep, all with true cfgs so everything survives.

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
    // WIFI(0) CLOCK(1) HTTP(2) WORKERS0(3) WORKERS1(4) SENSOR(5) BUTTON(6).
    assert_eq!(GRAPH.nodes.len(), 7);
    assert!(GRAPH.nodes.iter().all(|n| n.is_some()));

    assert_eq!(GRAPH.deps[0].len(), 0);
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
    assert_eq!(GRAPH.deps[2], [0u8].as_slice());
    assert_eq!(GRAPH.deps[3], [0u8].as_slice());
    assert_eq!(GRAPH.deps[4], [0u8].as_slice());
    assert_eq!(GRAPH.deps[5], [1u8].as_slice()); // SENSOR -> CLOCK
    assert_eq!(GRAPH.deps[6].len(), 0);
    assert_eq!(GRAPH.pools.len(), 1);

    for (pos, &n) in GRAPH.order.iter().enumerate() {
        for &d in GRAPH.deps[n as usize] {
            let dep_pos = GRAPH.order.iter().position(|&x| x == d).unwrap();
            assert!(dep_pos < pos);
        }
    }
}
