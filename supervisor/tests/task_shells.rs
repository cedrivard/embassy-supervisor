mod support;

use std::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph};
use support::settle;

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
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn pool_worker(node: &'static TaskNode) {
    POOL_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node FAST = Terminate, deps: [], task: worker::<Fast>(1);
    node SLOW = Terminate, deps: [FAST], task: worker::<Slow>(1);
    pool CRUNCH = [Terminate, OnDemand], deps: [],
        task: pool_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    // start(): both shell nodes + the pool floor (its shell is sized K=2) come up.
    sup.start(&spawner).await.expect("start");
    settle("all three shells ran once", || {
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

    sup.teardown().await.expect("teardown");
    assert!(
        !FAST.is_running() && !SLOW.is_running(),
        "shell nodes torn down"
    );

    sup.respawn_terminate(&spawner).await.expect("respawn");
    settle("all three shells ran twice", || {
        FAST_RUNS.load(Ordering::SeqCst) == 2
            && SLOW_RUNS.load(Ordering::SeqCst) == 2
            && POOL_RUNS.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_eq!(FAST_RUNS.load(Ordering::SeqCst), 2, "Fast shell respawned");
    assert_eq!(SLOW_RUNS.load(Ordering::SeqCst), 2, "Slow shell respawned");
    assert_eq!(
        POOL_RUNS.load(Ordering::SeqCst),
        2,
        "the pool floor's shell respawned with them"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn generated_shells_spawn_ack_and_respawn() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (fast={}, slow={}, pool={})",
                FAST_RUNS.load(Ordering::SeqCst),
                SLOW_RUNS.load(Ordering::SeqCst),
                POOL_RUNS.load(Ordering::SeqCst),
            )
        },
    );
}
