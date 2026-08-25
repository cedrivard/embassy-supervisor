use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Mode, NodeCfg, Supervisor, TaskNode, Topology, shape, supervisor_graph};

static SPAWNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode) {
    SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

// No `deps:` anywhere: the macro must pick `Flat`, and nothing structural is
// declared, so every shape bit must be clear.
supervisor_graph! {
    node A = Terminate, task: worker;
    node B = Terminate, task: worker;
    node C = Terminate, task: worker;
}

// One edge plus featureless structure: `Ordered`, with exactly the matching
// shape bits set. Declared only — never started (the parked `Pause` node and
// the empty resource slot would gate a real bring-up).
supervisor_graph! {
    name: SHAPED;
    executor EXEC;
    node ROOT = Terminate, task: worker;
    node GATED = Terminate, deps: [ROOT], executor: EXEC,
        resources: [SLOT: consume u8], task: shaped_gated_task;
    node PARKED = Pause;
    node LAZY = OnDemand, task: worker;
}

async fn shaped_gated_task(node: &'static TaskNode, _slot: u8) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

static HAND_CFG: NodeCfg = NodeCfg::new("hand", Mode::Terminate, None);
static HAND: TaskNode = TaskNode::new(&HAND_CFG, true);

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
    sup.start(&spawner).await.expect("start");
    settle(|| SPAWNS.load(Ordering::SeqCst) == 3).await;
    assert!(A.is_running() && B.is_running() && C.is_running());

    sup.teardown().await.expect("teardown");
    assert!(!A.is_running() && !B.is_running() && !C.is_running());

    sup.respawn_terminate(&spawner).await.expect("respawn");
    settle(|| SPAWNS.load(Ordering::SeqCst) == 6).await;
    assert!(A.is_running() && B.is_running() && C.is_running());

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn flat_graph_lifecycle_and_shape() {
    let _clock = embassy_time::MockDriver::get();

    assert_eq!(core::mem::size_of::<GRAPH_TOPOLOGY>(), 0);
    assert_eq!(<GRAPH_TOPOLOGY as Topology<3>>::SHAPE, 0);
    assert!(GRAPH.order().eq(0..3));
    assert!((0..3).all(|i| GRAPH.deps_of(i).is_empty()));

    assert_ne!(core::mem::size_of::<SHAPED_TOPOLOGY>(), 0);
    let bits = <SHAPED_TOPOLOGY as Topology<4>>::SHAPE;
    assert_eq!(
        bits,
        shape::EXEC_SLOTS | shape::RESOURCES | shape::PAUSE | shape::ON_DEMAND,
        "{bits:#b}"
    );
    assert_eq!(SHAPED.deps_of(1), &[0]);
    let pos = |i: u8| SHAPED.order().position(|x| x == i).unwrap();
    assert!(pos(0) < pos(1));

    assert_eq!(HAND.name(), "hand");
    assert!(matches!(HAND.mode(), Mode::Terminate));
    assert!(HAND.is_disabled());
    assert_eq!(shape::ALL, u32::MAX);

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
            "did not complete (spawns={})",
            SPAWNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
