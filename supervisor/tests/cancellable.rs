//! Behavioral tests for the `run_cancellable` combinators: completion returns
//! `Ok(output)`, a stop request wins the race as `Err(Aborted)`, and the
//! `_acked` variant completes the shutdown handshake by itself — `stop_node`
//! returns without the body ever touching `ack_dropped`. Same harness as
//! `teardown.rs`: one real executor on a std thread.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Aborted, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;

supervisor_graph! {
    node WORKER = Terminate, deps: [], task: worker;
}

/// Feeds the worker's `run_cancellable`d work future.
static WORK: Signal<CriticalSectionRawMutex, u32> = Signal::new();
/// Records each `run_cancellable` outcome: value on Ok, u32::MAX on Aborted.
static OUTCOME: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static ROUNDS: AtomicU32 = AtomicU32::new(0);
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// One combinator round per loop: race the WORK signal against shutdown. On
/// `Ok` keep looping; on `Err(Aborted)` fall through to `run_cancellable_acked`
/// parked on a pending future, whose ack is the only handshake this body has.
async fn worker(node: &'static TaskNode) {
    loop {
        ROUNDS.fetch_add(1, Ordering::SeqCst);
        match node.run_cancellable(WORK.wait()).await {
            Ok(v) => OUTCOME.signal(v),
            Err(Aborted) => {
                OUTCOME.signal(u32::MAX);
                // No cleanup between select and ack: the _acked variant both
                // proves the immediate-return path (shutdown already requested,
                // wait_shutdown's fast path) and completes the handshake.
                let _ = node
                    .run_cancellable_acked(core::future::pending::<()>())
                    .await;
                return;
            }
        }
    }
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(spawner).await.expect("start");
    settle(|| ROUNDS.load(Ordering::SeqCst) == 1).await;

    // ── work completes: Ok(output) carries the future's value ───────────────
    WORK.signal(7);
    settle(|| OUTCOME.signaled()).await;
    assert_eq!(OUTCOME.try_take(), Some(7), "Ok path carries the output");
    settle(|| ROUNDS.load(Ordering::SeqCst) == 2).await;
    PHASE.store(1, Ordering::SeqCst);

    // ── shutdown wins the race: Err(Aborted), and the _acked variant is the
    //    only ack in the body — stop_node completing proves it sufficed ───────
    sup.stop_node(&WORKER)
        .await
        .expect("acked by the combinator");
    assert_eq!(
        OUTCOME.try_take(),
        Some(u32::MAX),
        "stop surfaced as Err(Aborted) inside the body"
    );
    assert!(!WORKER.is_running());
    assert_eq!(
        ROUNDS.load(Ordering::SeqCst),
        2,
        "no extra round after the abort"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn run_cancellable_races_and_acks() {
    let _clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (phase={}, rounds={})",
            PHASE.load(Ordering::SeqCst),
            ROUNDS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
