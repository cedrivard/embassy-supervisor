//! Behavioral test for the `resources:` kind markers.
//!
//! * `consume` — the worker receives the value BY VALUE and drops it at
//!   teardown; the shell emits no restore, so the slot is EMPTY afterwards and a
//!   respawn without a fresh `provide()` fail-closes with `SpawnError::Busy`
//!   (the supervisor's bounded gate wait times out). Re-providing makes the next
//!   respawn succeed — the consume-then-re-provide wake pattern.
//! * `local` — a `!Send` value (`Rc<Cell<u32>>`) rides the graph-site local slot
//!   through start -> teardown -> respawn; the counter inside the `Rc`
//!   accumulating across the respawn proves the SAME instance was restored and
//!   re-taken (`ResourceSlot` itself cannot hold an `Rc`: its gate requires
//!   `T: Send`).
//!
//! Unlike the other suites this one needs mock TIME to advance: the Busy path is
//! a real `with_timeout(SLOT_READY_TIMEOUT, ..)` expiring, so the test thread
//! ticks `MockDriver` while the executor thread awaits.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::{SpawnError, Spawner};
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver};

/// The consumed resource. `Drop` is the observable: the worker owning (and
/// dropping) it at teardown is exactly what `consume` exists for — a stand-in
/// for a driver whose `Drop` releases pins/DMA.
struct Probe;

impl Drop for Probe {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

static DROPPED: AtomicU32 = AtomicU32::new(0);
static CONS_RUNS: AtomicU32 = AtomicU32::new(0);
static LOC_RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// `consume` worker: owns the Probe outright. Returning drops it — no restore.
async fn cons_worker(node: &'static TaskNode, _probe: Probe) {
    CONS_RUNS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
}

/// `local` worker: the `!Send` counter arrives `&mut` and is restored on exit,
/// so the value observed on the next run is the same allocation.
async fn loc_worker(node: &'static TaskNode, counter: &mut Rc<Cell<u32>>) {
    counter.set(counter.get() + 1);
    LOC_RUNS.store(counter.get(), Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
}

supervisor_graph! {
    // CONS first: both nodes are dep-free, so the topological order follows the
    // declaration order and the Busy respawn below fails on CONS BEFORE the loop
    // reaches (and double-spawns) LOC — `respawn_terminate` propagates the first
    // error and does not skip already-running nodes on a retry.
    node CONS = Terminate, deps: [], task: cons_worker,
        resources: [PROBE: consume Probe];
    node LOC = Terminate, deps: [], task: loc_worker,
        resources: [COUNTER: local Rc<Cell<u32>>];
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

    // main's provide(): both slots filled before start.
    PROBE.provide(Probe);
    COUNTER.provide(Rc::new(Cell::new(0)));

    sup.start(spawner).await.expect("start");
    settle(|| CONS_RUNS.load(Ordering::SeqCst) == 1 && LOC_RUNS.load(Ordering::SeqCst) == 1).await;

    sup.teardown().await;
    // consume: the worker dropped the Probe and the shell restored nothing.
    assert_eq!(
        DROPPED.load(Ordering::SeqCst),
        1,
        "worker dropped the Probe"
    );
    assert!(PROBE.take().is_none(), "consume leaves the slot empty");

    // Respawn WITHOUT re-providing: the gate wait times out (mock time is
    // ticking, see the test thread) and the spawn fail-closes.
    assert!(
        matches!(sup.respawn_terminate(spawner).await, Err(SpawnError::Busy)),
        "respawn without a fresh provide must fail Busy"
    );

    // Re-provide (the wake pattern: build a fresh instance each cycle) and
    // respawn: CONS gets the new Probe, LOC re-takes the SAME restored Rc.
    PROBE.provide(Probe);
    sup.respawn_terminate(spawner).await.expect("respawn");
    settle(|| LOC_RUNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        CONS_RUNS.load(Ordering::SeqCst),
        2,
        "consume node respawned"
    );
    assert_eq!(
        LOC_RUNS.load(Ordering::SeqCst),
        2,
        "local respawn re-took the SAME Rc (counter accumulated)"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn resource_kinds_consume_and_local() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    // Tick mock time so the Busy path's `with_timeout(SLOT_READY_TIMEOUT)` can
    // actually expire (the other waits are already-satisfied and never park).
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (cons={}, loc={}, dropped={})",
            CONS_RUNS.load(Ordering::SeqCst),
            LOC_RUNS.load(Ordering::SeqCst),
            DROPPED.load(Ordering::SeqCst),
        );
        clock.advance(Duration::from_millis(10));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
