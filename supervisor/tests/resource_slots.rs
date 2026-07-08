//! Behavioral test for `resources:` — safe resource threading. main() MOVES an
//! owned value into a macro-emitted `ResourceSlot`; the generated glue takes it
//! before the spawn, the shell lends the worker `&mut T` and restores it after
//! the worker returns. The proof of identity: a counter INSIDE the resource keeps
//! incrementing across `start -> teardown -> respawn_terminate` — a re-acquired
//! (fresh) instance would reset to zero.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

/// The threaded resource: owned, non-Copy, stateful — a stand-in for a HAL
/// driver (`Output`, `Peri<'static, USB>`, ...). `runs` living INSIDE the value
/// is what distinguishes "same instance restored and re-taken" from "fresh
/// instance re-acquired": only the former accumulates.
struct Probe {
    runs: u32,
}

/// Mirrors the resource's internal counter so the test thread can observe it
/// (the resource itself is inside the slot/task, not shareable).
static OBSERVED_RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Plain async worker (not a `#[embassy_executor::task]`): receives the node
/// plus `&mut` of the threaded resource, exactly as the generated shell calls it.
async fn worker(node: &'static TaskNode, probe: &mut Probe) {
    probe.runs += 1;
    OBSERVED_RUNS.store(probe.runs, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
    // Returning here is the restore point: the shell puts the Probe back into
    // PROBE for the next spawn.
}

supervisor_graph! {
    node METER = Terminate, deps: [], task: worker,
        resources: [PROBE: Probe];
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

    // start(): the glue takes the provided Probe (runs == 0) and the shell hands
    // it to the worker.
    sup.start(spawner).await.expect("start");
    settle(|| OBSERVED_RUNS.load(Ordering::SeqCst) == 1).await;
    assert_eq!(
        OBSERVED_RUNS.load(Ordering::SeqCst),
        1,
        "first run saw the fresh resource"
    );

    // teardown(): the worker acks and returns; the shell restores the Probe to
    // its slot. The restore happens on task exit, so wait for the running flag.
    sup.teardown().await;
    assert!(!METER.is_running(), "node torn down");

    // respawn_terminate(): the supervisor awaits the slot being restored, the
    // glue re-takes it — the SAME instance, so the internal counter continues
    // (a re-acquired fresh Probe would report 1 again, not 2).
    sup.respawn_terminate(spawner).await.expect("respawn");
    settle(|| OBSERVED_RUNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        OBSERVED_RUNS.load(Ordering::SeqCst),
        2,
        "respawn re-took the SAME resource instance (counter accumulated)"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn threaded_resource_round_trips_across_respawn() {
    // Frozen mock clock: the ack-timeout / slot-wait Timers need a registered
    // driver but never fire (acks are immediate, the slot is always filled).
    let _clock = MockDriver::get();

    // Unit-level slot protocol checks (host-visible without an executor).
    assert!(PROBE.take().is_none(), "unprovided slot is empty");
    PROBE.provide(Probe { runs: 0 });
    let p = PROBE.take().expect("provided value is takeable");
    assert!(PROBE.take().is_none(), "take empties the slot");
    PROBE.restore(p);

    // ^ the Probe (runs == 0) is back in the slot: this is main's provide(),
    // done BEFORE the supervisor starts — the graph takes it from here.
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
            "did not complete (runs={})",
            OBSERVED_RUNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
