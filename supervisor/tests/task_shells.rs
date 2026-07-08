//! Behavioral test for `task:` — macro-generated `#[embassy_executor::task]`
//! shells around ONE generic async worker fn.
//! A real executor drives two shell nodes instantiated at different
//! types plus a shell-driven pool floor through `start -> teardown ->
//! respawn_terminate`, proving each stamped shell spawns, acks, and respawns like
//! a hand-written task, and that the per-declaration `TaskPool`s are independent.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

trait Probe {
    fn counter() -> &'static AtomicU32;
}
struct Fast;
struct Slow;
static FAST_RUNS: AtomicU32 = AtomicU32::new(0);
static SLOW_RUNS: AtomicU32 = AtomicU32::new(0);
impl Probe for Fast {
    fn counter() -> &'static AtomicU32 {
        &FAST_RUNS
    }
}
impl Probe for Slow {
    fn counter() -> &'static AtomicU32 {
        &SLOW_RUNS
    }
}

static POOL_RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The ONE generic worker: not a `#[embassy_executor::task]` — the graph below
/// stamps a concrete shell per declaration. Counts its runs per instantiated
/// type, then behaves like a well-mannered Terminate task.
async fn worker<P: Probe>(node: &'static TaskNode, bump: u32) {
    P::counter().fetch_add(bump, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
}

/// Pool-member worker (non-generic on purpose: proves `task:` is useful for the
/// plain "spare me the #[task] boilerplate" case too).
async fn pool_worker(node: &'static TaskNode) {
    POOL_RUNS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
}

supervisor_graph! {
    node FAST = Terminate, deps: [], task: worker::<Fast>(1);
    // `pool_size: 2` exercises the knob end-to-end (shell TaskPool of 2).
    node SLOW = Terminate, deps: [FAST], task: worker::<Slow>(1), pool_size: 2;
    pool CRUNCH = [Terminate, OnDemand], deps: [],
        task: pool_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
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

    // start(): both shell nodes + the pool floor (its shell is sized K=2) come up.
    sup.start(spawner).await.expect("start");
    settle(|| {
        FAST_RUNS.load(Ordering::SeqCst) == 1
            && SLOW_RUNS.load(Ordering::SeqCst) == 1
            && POOL_RUNS.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(
        FAST_RUNS.load(Ordering::SeqCst),
        1,
        "Fast instantiation ran"
    );
    assert_eq!(
        SLOW_RUNS.load(Ordering::SeqCst),
        1,
        "Slow instantiation ran"
    );
    assert_eq!(POOL_RUNS.load(Ordering::SeqCst), 1, "pool floor shell ran");
    #[cfg(feature = "trace")]
    {
        // The generated glue keeps the call -> adopt -> spawn shape, so the
        // shell's task id is registered on the node.
        assert_ne!(FAST.task_id(), 0, "shell spawn adopted into trace registry");
    }

    // teardown(): shells select against wait_shutdown and ack like any task.
    sup.teardown().await;
    assert!(
        !FAST.is_running() && !SLOW.is_running(),
        "shell nodes torn down"
    );

    // respawn_terminate(): each shell's TaskPool slot was freed on exit, so the
    // same shells respawn; per-type counters prove the right monomorphizations.
    sup.respawn_terminate(spawner).await.expect("respawn");
    settle(|| FAST_RUNS.load(Ordering::SeqCst) == 2 && SLOW_RUNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(FAST_RUNS.load(Ordering::SeqCst), 2, "Fast shell respawned");
    assert_eq!(SLOW_RUNS.load(Ordering::SeqCst), 2, "Slow shell respawned");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn generated_shells_spawn_ack_and_respawn() {
    // Frozen mock clock: the teardown ack-timeout Timer needs a registered driver
    // but never fires (every shell acks immediately).
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
            "did not complete (fast={}, slow={}, pool={})",
            FAST_RUNS.load(Ordering::SeqCst),
            SLOW_RUNS.load(Ordering::SeqCst),
            POOL_RUNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
