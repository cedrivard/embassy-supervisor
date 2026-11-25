//! `task-priority`: a stopped `spawn:` task below a driver at the supervisor's
//! priority (a `run()` task, or a hand-written driver that raised itself), still
//! in its final poll. Under the default supervisor priority, 255, the driver is
//! polled before that poll, so the respawn first fails `Busy`, and its yield is
//! polled again before the task. The retry then backs off at the supervisor's
//! priority, the task returns meanwhile, and the respawn succeeds once the
//! clock moves. A retry that spun at 255 would never let the task return.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use embassy_executor::{Metadata, Spawner};
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

supervisor_graph! {
    node X = Terminate, deps: [], spawn: late_return, slot_timeout: 5000;
}

static STARTS: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// Acks its stop, then takes one more poll before it returns.
#[embassy_executor::task]
async fn late_return(node: &'static TaskNode) {
    STARTS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
    embassy_futures::yield_now().await;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("boot");
    support::settle("X started", || STARTS.load(Ordering::SeqCst) == 1).await;
    let me = Metadata::for_current_task().await;
    me.set_priority(GRAPH.supervisor_priority);
    sup.stop_node(&X).await.expect("X acks");
    sup.start_node(&X, &spawner).await.expect("respawn");
    me.set_priority(0);
    support::settle("X restarted", || STARTS.load(Ordering::SeqCst) == 2).await;
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_busy_respawn_lets_the_dying_task_return() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Free(embassy_time::Duration::from_millis(1)),
        10,
        || format!("starts {}", STARTS.load(Ordering::SeqCst)),
    );
}
