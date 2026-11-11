//! A spawn that stays `Busy` gets one pass of its executor, then sleeps a
//! backoff between retries: with the clock frozen it is attempted exactly
//! twice, however long the executor keeps running, and it faults once the
//! clock passes its gate budget.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration as StdDuration;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{FaultKind, Supervisor, supervisor_graph};
use embassy_time::{Duration, MockDriver};

static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
/// Set once the frozen-clock phase is over: the test thread then moves the
/// clock past X's gate budget.
static ADVANCE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

supervisor_graph! {
    node X = Terminate, deps: [], slot_timeout: 100, spawn: |_s| {
        crate::ATTEMPTS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        Err(embassy_executor::SpawnError::Busy)
    };
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    let mut start = core::pin::pin!(sup.start_node(&X, &spawner));
    let script = async {
        for _ in 0..1000 {
            embassy_futures::yield_now().await;
        }
    };
    if let Either::First(r) = select(start.as_mut(), script).await {
        panic!("start_node returned with the clock frozen: {r:?}");
    }
    assert_eq!(
        ATTEMPTS.load(Ordering::SeqCst),
        2,
        "first attempt, one executor pass, second attempt, then parked on the timer"
    );

    ADVANCE.store(true, Ordering::SeqCst);
    let fault = start.await.expect_err("X never spawns");
    assert!(matches!(fault.kind, FaultKind::Spawn(_)), "{fault}");
    assert!(!X.is_running());
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_busy_spawn_backs_off_instead_of_spinning() {
    // Once ADVANCE is set, move the clock past X's gate budget, once.
    let clock = MockDriver::get();
    std::thread::spawn(move || {
        while !ADVANCE.load(Ordering::SeqCst) {
            std::thread::sleep(StdDuration::from_millis(5));
        }
        clock.advance(Duration::from_millis(200));
    });
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (attempts={})",
                ATTEMPTS.load(Ordering::SeqCst)
            )
        },
    );
}
