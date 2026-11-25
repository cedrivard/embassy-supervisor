//! `task-priority`: the supervisor's loops that never return on their own
//! raise their task to the graph's supervisor priority and leave it there.
//! They never lower a task, so one task that drives two graphs runs at the
//! higher of the two, whatever the order.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::{Metadata, Spawner};
use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

supervisor_graph! {
    supervisor priority: 9;
    node A = Terminate, deps: [], spawn: idle;
}

supervisor_graph! {
    name: SCAN;
    supervisor priority: 20;
    node C = Terminate, deps: [], spawn: idle;
}

static DONE: AtomicBool = AtomicBool::new(false);

#[embassy_executor::task(pool_size = 2)]
async fn idle(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.ack_dropped();
}

async fn own_priority() -> u8 {
    Metadata::for_current_task().await.priority()
}

/// The task's priority once `drivers` had their first poll.
async fn after_first_poll<T>(drivers: impl core::future::Future<Output = T>) -> u8 {
    match select(drivers, own_priority()).await {
        Either::First(_) => panic!("a driver returned"),
        Either::Second(p) => p,
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let main = Supervisor::new(&GRAPH);
    let scan = Supervisor::new(&SCAN);

    assert_eq!(after_first_poll(main.run(&spawner)).await, 9);
    assert_eq!(own_priority().await, 9, "kept after `run` is gone");

    // The higher graph first: the lower one does not pull the task down.
    Metadata::for_current_task().await.set_priority(0);
    let both = join(scan.run(&spawner), main.run(&spawner));
    assert_eq!(after_first_poll(both).await, 20);

    // The other loops that never return do the same.
    Metadata::for_current_task().await.set_priority(0);
    assert_eq!(after_first_poll(main.run_pools(&spawner)).await, 9);
    Metadata::for_current_task().await.set_priority(0);
    assert_eq!(after_first_poll(main.monitor()).await, 9);

    // A task already above both keeps its priority.
    Metadata::for_current_task().await.set_priority(50);
    let both = join(main.run(&spawner), scan.run(&spawner));
    assert_eq!(after_first_poll(both).await, 50);
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn the_driver_loops_raise_their_task_and_never_lower_it() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || format!("A {} C {}", A.is_running(), C.is_running()),
    );
}
