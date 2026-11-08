//! The driver gives one queued control command a turn after each bind
//! cascade. `select3` polls the bind wait first, so a provider that re-latches
//! it while every cascade waits would otherwise win every iteration: here
//! LINK flaps on every executor pass while each cascade's restart of SESSION
//! waits out its gate budget on AUX, and a `Deactivate` queued for OTHER must
//! still run.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{ControlOp, Supervisor, TaskNode, supervisor_graph, try_request_control};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node LINK = Terminate, deps: [], task: provider;
    node AUX = Terminate, deps: [], task: provider;
    node SESSION = Terminate, deps: [LINK ready bound, AUX ready], task: parked,
        slot_timeout: 20;
    node OTHER = Terminate, deps: [], task: parked;
}

static FLAP: AtomicBool = AtomicBool::new(false);
static FLAP_STOP: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static FAULT: Mutex<Option<String>> = Mutex::new(None);

async fn until(cond: impl Fn() -> bool) {
    while !cond() {
        embassy_futures::yield_now().await;
    }
}

async fn provider(node: &'static TaskNode) {
    node.set_ready();
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn parked(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

/// Withdraws and restores LINK's readiness within one poll, once per pass:
/// the driver always finds LINK serving, and the bind wait is latched again
/// whenever the driver waits.
#[embassy_executor::task]
async fn flapper() {
    until(|| FLAP.load(Ordering::SeqCst)).await;
    while !FLAP_STOP.load(Ordering::SeqCst) {
        LINK.clear_ready();
        LINK.set_ready();
        embassy_futures::yield_now().await;
    }
}

async fn script() {
    let clock = MockDriver::get();
    until(|| SESSION.is_running() && OTHER.is_running()).await;
    // AUX is not a bound provider: withdrawing it leaves SESSION running and
    // only gates SESSION's next start.
    AUX.clear_ready();
    LINK.clear_ready();
    until(|| SESSION.is_bound_stopped() && !SESSION.is_running()).await;

    // Each flap latches the bind wait with LINK serving, so every cascade
    // restarts SESSION, which then waits for AUX. The clock stands still, so
    // the first restart is held in that wait while the command is queued.
    FLAP.store(true, Ordering::SeqCst);
    for _ in 0..16 {
        embassy_futures::yield_now().await;
    }
    try_request_control(&OTHER, ControlOp::Deactivate).expect("mailbox has room");

    // Device time passes from here on, 1 ms per pass: each restart spends
    // SESSION's 20 ms budget and fails, and the next flap has already
    // latched the bind wait again.
    while !(OTHER.is_disabled() && !OTHER.is_running()) {
        clock.advance(Duration::from_millis(1));
        embassy_futures::yield_now().await;
    }
    FLAP_STOP.store(true, Ordering::SeqCst);
    assert!(
        SESSION.is_bound_stopped() && !SESSION.is_running(),
        "SESSION's restarts kept waiting on AUX"
    );
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    if let Either::First(fault) = select(sup.run(&spawner), script()).await {
        *FAULT.lock().unwrap() = Some(fault.to_string());
    }
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_flapping_provider_cannot_starve_a_queued_command() {
    let _clock = MockDriver::get();
    let thread = std::thread::spawn(|| {
        let executor = Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(runner(spawner).unwrap());
            spawner.spawn(flapper().unwrap());
        });
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        if thread.is_finished() {
            thread.join().expect("executor assertions failed");
            panic!("executor exited before completion");
        }
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(2));
    }
    assert_eq!(*FAULT.lock().unwrap(), None, "run() returned a fault");
}
