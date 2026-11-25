//! `task-priority`: a `Busy` respawn on the supervisor's own executor waits at
//! the supervisor's priority, bounded by its deadline. `SPIN` yields at
//! priority 200 until it is stopped, and `X` acks its stop at 250, then takes
//! one more poll before it returns. `Restart X` and then `Deactivate SPIN` must
//! both be applied. A supervisor that dropped below 200 to let `X` return
//! would never be polled again while `SPIN` yields.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use embassy_executor::{Metadata, Spawner};
use embassy_supervisor::{ControlOp, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

supervisor_graph! {
    node SPIN = Terminate, deps: [], priority: 200, task: spinner;
    node X = Terminate, deps: [], spawn: late_return, slot_timeout: 5000;
}

/// Lets `SPIN` start spinning once `X` is up.
static GO: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static SPINNING: AtomicBool = AtomicBool::new(false);
static X_STARTS: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn spinner(node: &'static TaskNode) {
    GO.wait().await;
    SPINNING.store(true, Ordering::SeqCst);
    while !node.shutdown_requested() {
        embassy_futures::yield_now().await;
    }
    node.ack_dropped();
}

/// Runs above `SPIN`, acks its stop, then takes one more poll before it
/// returns.
#[embassy_executor::task]
async fn late_return(node: &'static TaskNode) {
    Metadata::for_current_task().await.set_priority(250);
    if X_STARTS.fetch_add(1, Ordering::SeqCst) == 1 {
        DONE.store(true, Ordering::SeqCst);
    }
    node.wait_shutdown().await;
    node.ack_dropped();
    embassy_futures::yield_now().await;
}

#[embassy_executor::task]
async fn supervisor_task(spawner: Spawner) {
    let fault = Supervisor::new(&GRAPH).run(&spawner).await;
    panic!("supervisor returned: {fault}");
}

fn wait_for(flag: impl Fn() -> bool) {
    while !flag() {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_busy_respawn_keeps_the_supervisor_ahead_of_a_spinning_node() {
    std::thread::spawn(|| {
        wait_for(|| X_STARTS.load(Ordering::SeqCst) == 1);
        GO.signal(());
        wait_for(|| SPINNING.load(Ordering::SeqCst));
        for op in [(&X, ControlOp::Restart), (&SPIN, ControlOp::Deactivate)] {
            embassy_supervisor::try_request_control(op.0, op.1)
                .expect("the control queue has room");
        }
    });
    // `X` comes back at priority 0, below `SPIN`: its second start shows that
    // the restart finished and the deactivation after it was applied.
    support::run(
        |spawner| spawner.spawn(supervisor_task(spawner).unwrap()),
        &DONE,
        support::Clock::Free(embassy_time::Duration::from_millis(1)),
        10,
        || {
            format!(
                "x starts {}, spinning {}, SPIN running {}",
                X_STARTS.load(Ordering::SeqCst),
                SPINNING.load(Ordering::SeqCst),
                SPIN.is_running()
            )
        },
    );
    assert!(!SPIN.is_running());
}
