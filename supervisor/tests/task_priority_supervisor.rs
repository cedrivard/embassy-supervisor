//! `task-priority` with the default supervisor priority, 255: the supervisor
//! stays ahead of a node that never stops yielding at priority 200, so a
//! control command still reaches it. At priority 0 the supervisor would never
//! be polled again and this scenario would pass its deadline.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use embassy_executor::Spawner;
use embassy_supervisor::{ControlOp, Supervisor, TaskNode, supervisor_graph};

supervisor_graph! {
    node SPIN = Terminate, deps: [], priority: 200, task: spinner;
}

static STARTED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn spinner(node: &'static TaskNode) {
    STARTED.store(true, Ordering::SeqCst);
    while !node.shutdown_requested() {
        embassy_futures::yield_now().await;
    }
    node.ack_dropped();
    DONE.store(true, Ordering::SeqCst);
}

#[embassy_executor::task]
async fn supervisor_task(spawner: Spawner) {
    let fault = Supervisor::new(&GRAPH).run(&spawner).await;
    panic!("supervisor returned: {fault}");
}

#[test]
fn the_supervisor_outranks_a_busy_node() {
    std::thread::spawn(|| {
        while !STARTED.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        embassy_supervisor::try_request_control(&SPIN, ControlOp::Deactivate)
            .expect("the control queue has room");
    });
    support::run(
        |spawner| spawner.spawn(supervisor_task(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || format!("started {}", STARTED.load(Ordering::SeqCst)),
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while SPIN.is_running() {
        assert!(Instant::now() < deadline, "SPIN still marked running");
        std::thread::sleep(Duration::from_millis(1));
    }
}
