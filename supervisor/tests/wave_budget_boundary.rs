//! A stop wave stamps each signal in whole milliseconds since the wave began.
//! A node signalled mid-millisecond still gets its full `ack_timeout`: an ack
//! that lands one tick before `signal + ack_timeout` is on time, so the wave
//! must not have faulted the node at the millisecond the stamp was cut down to.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver};
use support::{settle, yields};

static USER_GO: AtomicBool = AtomicBool::new(false);
static DEP_GO: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

/// Answers a stop only once the script lets it.
async fn gated_worker(node: &'static TaskNode, go: &AtomicBool) {
    node.wait_shutdown().await;
    settle("go", || go.load(Ordering::SeqCst)).await;
}

async fn dep_worker(node: &'static TaskNode) {
    gated_worker(node, &DEP_GO).await;
}

async fn user_worker(node: &'static TaskNode) {
    gated_worker(node, &USER_GO).await;
}

supervisor_graph! {
    node DEP = Terminate, deps: [], task: dep_worker, ack_timeout: 100;
    node USER = Terminate, deps: [DEP], task: user_worker, ack_timeout: 100;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    settle("both up", || DEP.is_running() && USER.is_running()).await;

    // Half a millisecond, in ticks: the clock runs at `TICK_HZ`.
    let half_ms = Duration::from_ticks(embassy_time::TICK_HZ / 2000);
    let tick = Duration::from_ticks(1);
    let script = async {
        // The wave signals USER first (DEP is held by its dependent).
        settle("USER signalled", || USER.shutdown_requested()).await;
        assert!(!DEP.shutdown_requested(), "DEP is held by USER");
        // USER acks half a millisecond into the wave, so DEP's signal is
        // stamped mid-millisecond.
        clock.advance(half_ms);
        USER_GO.store(true, Ordering::SeqCst);
        settle("DEP signalled", || DEP.shutdown_requested()).await;
        // One tick short of DEP's full window: the wave must still wait.
        clock.advance(Duration::from_millis(100) - tick);
        yields(1_000).await;
        DEP_GO.store(true, Ordering::SeqCst);
        core::future::pending::<()>().await;
    };
    match select(sup.teardown(), script).await {
        Either::First(r) => {
            r.expect("DEP acked inside its 100 ms window");
            assert!(
                DEP_GO.load(Ordering::SeqCst),
                "the wave ended before DEP was let ack"
            );
        }
        Either::Second(()) => unreachable!("the script never ends"),
    }
    assert!(!DEP.is_running() && !USER.is_running());
    DONE.store(true, Ordering::SeqCst);
}

// The script steps half a millisecond: the clock must resolve it.
const _: () = assert!(embassy_time::TICK_HZ / 2000 > 0);

#[test]
fn a_signal_stamped_mid_millisecond_keeps_its_full_ack_window() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (dep_running={}, user_running={})",
                DEP.is_running(),
                USER.is_running(),
            )
        },
    );
}
