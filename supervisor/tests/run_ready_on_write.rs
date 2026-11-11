//! `Supervisor::run` sweeps from its first poll: a `ready_on_write` provider
//! started late in the boot wave is probed from its start, at a fraction of
//! the default `slot_timeout`, so its first write asserts readiness inside
//! its dependent's bring-up gate.

mod support;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver, Timer};

pub static PRODUCED: AtomicU32 = AtomicU32::new(0);
static DEP_STARTS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);
static FAULT: Mutex<Option<String>> = Mutex::new(None);

supervisor_graph! {
    observe writes: it.load(core::sync::atomic::Ordering::Relaxed);
    node PRODUCER = Terminate, deps: [], task: producer, beat_timeout: 1000, ready_on_write,
        resources: [SEED: u32], slot_timeout: 500, writes: [crate::PRODUCED observed beat];
    node DEPENDENT = Terminate, deps: [PRODUCER ready], task: dependent;
}

async fn until(cond: impl Fn() -> bool) {
    while !cond() {
        embassy_futures::yield_now().await;
    }
}

async fn producer(node: &'static TaskNode, _seed: &mut u32) {
    PRODUCED.fetch_add(1, Ordering::Relaxed);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn dependent(node: &'static TaskNode) {
    DEP_STARTS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// Fills the producer's slot 50 ms into the boot wave, so it starts while the
/// monitor sleeps out the down node's full budget.
#[embassy_executor::task]
async fn seeder() {
    Timer::after(Duration::from_millis(50)).await;
    SEED.provide(1);
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    if let Either::First(fault) = select(sup.run(&spawner), until(|| DEPENDENT.is_running())).await
    {
        *FAULT.lock().unwrap() = Some(fault.to_string());
    }
    DONE.store(true, Ordering::SeqCst);
}

/// Moves the mock clock 5 ms per executor pass until the test is done, so
/// device time passes only while the executor runs: a descheduled executor
/// thread cannot sleep through DEPENDENT's 100 ms `slot_timeout`.
#[embassy_executor::task]
async fn clock_ticker() {
    let clock = MockDriver::get();
    while !DONE.load(Ordering::SeqCst) {
        clock.advance(Duration::from_millis(5));
        embassy_futures::yield_now().await;
    }
}

#[test]
fn a_late_ready_on_write_provider_gates_its_dependent_at_boot() {
    support::run(
        |spawner| {
            spawner.spawn(runner(spawner).unwrap());
            spawner.spawn(seeder().unwrap());
            spawner.spawn(clock_ticker().unwrap());
        },
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
    assert_eq!(*FAULT.lock().unwrap(), None, "run() returned a fault");
    assert!(PRODUCER.is_ready(), "the first write asserted readiness");
    assert_eq!(DEP_STARTS.load(Ordering::SeqCst), 1);
}
