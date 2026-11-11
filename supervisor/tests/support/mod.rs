//! Shared harness for the executor scenarios: one leaked executor on its own
//! thread, a mock-clock policy, and a deadline that names what was pending.
#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_time::{Duration, MockDriver};

/// How the test thread moves the mock clock while the scenario runs.
pub enum Clock {
    /// The mock clock never moves from the test thread.
    Frozen,
    /// Advance by `step` every 2 ms of wall time.
    Free(Duration),
    /// Advance by `step` every 2 ms of wall time while the flag is set.
    While(&'static AtomicBool, Duration),
}

/// Set by the first `run` of a test binary.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Run one executor scenario to completion: spawn a leaked `Executor` on its
/// own thread, call `init` with its spawner, apply `clock`, and return when
/// `done` is set. An executor thread that dies re-raises its panic here; a
/// scenario that outlives `secs` (overridable by `SV_TEST_DEADLINE_SECS`)
/// fails with `diag()` in the message. One scenario per test binary.
pub fn run(
    init: impl FnOnce(Spawner) + Send + 'static,
    done: &'static AtomicBool,
    clock: Clock,
    secs: u64,
    diag: impl Fn() -> String,
) {
    assert!(
        !STARTED.swap(true, Ordering::SeqCst),
        "one executor scenario per test binary"
    );
    let (driver, flag, step) = match clock {
        Clock::Frozen => (None, None, Duration::from_ticks(0)),
        Clock::Free(step) => (Some(MockDriver::get()), None, step),
        Clock::While(flag, step) => (Some(MockDriver::get()), Some(flag), step),
    };
    let thread = std::thread::spawn(move || {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(init)
    });
    let secs = std::env::var("SV_TEST_DEADLINE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(secs);
    let deadline = StdInstant::now() + StdDuration::from_secs(secs);
    while !done.load(Ordering::SeqCst) {
        if thread.is_finished() {
            // `Executor::run` never returns: a finished thread has panicked.
            let Err(panic) = thread.join();
            std::panic::resume_unwind(panic);
        }
        assert!(
            StdInstant::now() < deadline,
            "scenario passed its {secs} s deadline: {}",
            diag()
        );
        if let Some(driver) = driver
            && flag.is_none_or(|f| f.load(Ordering::SeqCst))
        {
            driver.advance(step);
        }
        std::thread::sleep(StdDuration::from_millis(2));
    }
}

/// Yield until `f` holds; panics naming `what` after 100_000 yields.
pub async fn settle(what: &'static str, mut f: impl FnMut() -> bool) {
    for _ in 0..100_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
    assert!(f(), "did not settle: {what}");
}

/// Exactly `n` executor yields.
pub async fn yields(n: usize) {
    for _ in 0..n {
        embassy_futures::yield_now().await;
    }
}
