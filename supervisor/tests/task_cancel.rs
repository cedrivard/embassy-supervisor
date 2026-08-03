//! Behavioral test for `cancel` — the `task:` flag that makes the generated
//! shell own the shutdown race so the worker doesn't have to.
//!
//! The point of the flag is the worker signatures below: `looper` and `oneshot`
//! take no `&TaskNode`, name nothing from this crate, and are exactly the shape
//! an existing firmware already has (a plain `async fn` that loops forever).
//! What is proven here is that the shell still does everything it does for a
//! node-aware worker: `stop_node` is acked, the resource is restored for the
//! respawn, an `exit:` value is captured on a real completion — and NOT captured
//! when the worker was aborted instead.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;

/// Threaded resource, as in `resource_slots.rs`: the counter lives INSIDE the
/// value, so an accumulating count proves the same instance came back — i.e.
/// that the shell's restore tail ran even though the worker never returned on
/// its own.
struct Probe {
    runs: u32,
}

static OBSERVED_RUNS: AtomicU32 = AtomicU32::new(0);
static ROUNDS: AtomicU32 = AtomicU32::new(0);
static WORK: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static DONE: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU32 = AtomicU32::new(0);

/// A supervisor-unaware worker: diverging, no node argument, would never ack a
/// shutdown by itself. Under `cancel` the shell drops this future in place.
/// Resources still arrive, now as the FIRST argument (no node leads it).
async fn looper(probe: &mut Probe) -> ! {
    probe.runs += 1;
    OBSERVED_RUNS.store(probe.runs, Ordering::SeqCst);
    loop {
        ROUNDS.fetch_add(1, Ordering::SeqCst);
        WORK.wait().await;
    }
}

/// A `cancel` worker that DOES finish on its own: `exit:` captures its value,
/// proving the flag doesn't cost the completion path.
async fn oneshot() -> u32 {
    7
}

supervisor_graph! {
    node LOOPER = Terminate, deps: [], task: looper, cancel,
        resources: [PROBE: Probe];
    node ONESHOT = Terminate, deps: [], task: oneshot, cancel, exit: u32;
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
    settle(|| ROUNDS.load(Ordering::SeqCst) >= 1).await;
    assert_eq!(OBSERVED_RUNS.load(Ordering::SeqCst), 1, "first activation");

    // ── the completion path is untouched by the flag ────────────────────────
    assert_eq!(
        ONESHOT_EXIT.wait_take().await,
        7,
        "a cancel worker that returns still provides its exit value"
    );
    assert!(ONESHOT.has_exited(), "and still records the completion");
    PHASE.store(1, Ordering::SeqCst);

    // ── the ack: the worker contains no handshake at all, so `stop_node`
    //    returning is proof the shell supplied it ─────────────────────────────
    sup.stop_node(&LOOPER)
        .await
        .expect("acked by the generated shell, not by the worker");
    assert!(!LOOPER.is_running(), "stopped");
    assert!(LOOPER.has_exited(), "and recorded as exited");
    let rounds_at_stop = ROUNDS.load(Ordering::SeqCst);
    PHASE.store(2, Ordering::SeqCst);

    // ── the abort ran the shell's whole tail: the resource is back in its slot
    //    for the respawn, which is what `Terminate` promises ──────────────────
    sup.start_node(&LOOPER, spawner)
        .await
        .expect("respawn after cancel");
    settle(|| OBSERVED_RUNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        OBSERVED_RUNS.load(Ordering::SeqCst),
        2,
        "same Probe instance restored and re-taken (a fresh one would read 1)"
    );
    PHASE.store(3, Ordering::SeqCst);

    // ── the dropped future is really gone: no round advances after the abort
    //    even though its work signal fires again ──────────────────────────────
    let rounds_before = ROUNDS.load(Ordering::SeqCst);
    sup.stop_node(&LOOPER).await.expect("second stop");
    WORK.signal(1);
    settle(|| false).await;
    assert_eq!(
        ROUNDS.load(Ordering::SeqCst),
        rounds_before,
        "the aborted worker is dropped, not merely ignored"
    );
    assert!(rounds_at_stop >= 1);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn cancel_flag_races_acks_and_restores() {
    let _clock = MockDriver::get();
    PROBE.provide(Probe { runs: 0 });

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
            "did not complete (phase={}, rounds={}, runs={})",
            PHASE.load(Ordering::SeqCst),
            ROUNDS.load(Ordering::SeqCst),
            OBSERVED_RUNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
