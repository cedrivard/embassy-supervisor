use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver, Timer};

struct Gadget {
    generation: u32,
}

impl Drop for Gadget {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
struct Handle {
    generation: u32,
}

static GENERATION: AtomicU32 = AtomicU32::new(0);
static OWNER_GEN: AtomicU32 = AtomicU32::new(0);
static READ_SUM: AtomicU32 = AtomicU32::new(0);
static READ_RUNS: AtomicU32 = AtomicU32::new(0);
static DROPPED: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn provider_worker(node: &'static TaskNode) {
    Timer::after_millis(500).await;
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    GADGET.provide(Gadget { generation });
    HANDLE.provide(Handle { generation });
    let _ = node.run_cancellable(core::future::pending::<()>()).await;
    HANDLE.clear();
    node.ack_dropped();
}

/// `consume` consumer: owns the Gadget; returning after the ack drops it.
async fn owner_worker(node: &'static TaskNode, gadget: Gadget) {
    OWNER_GEN.store(gadget.generation, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn reader_worker(node: &'static TaskNode, handle: Handle) {
    READ_SUM.fetch_add(handle.generation, Ordering::SeqCst);
    READ_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    // First in topo order; the consumers' `deps:` guarantee it (re)spawns
    node PROVIDER = Terminate, deps: [], task: provider_worker;
    node OWNER = Terminate, deps: [PROVIDER], task: owner_worker,
        slot_timeout: 5000,
        resources: [GADGET: consume Gadget];
    node READ_A = Terminate, deps: [PROVIDER], task: reader_worker,
        slot_timeout: 5000,
        resources: [HANDLE: shared Handle];
    node READ_B = Terminate, deps: [PROVIDER], task: reader_worker,
        slot_timeout: 5000,
        resources: [HANDLE: shared Handle];
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..100_000 {
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
    settle(|| READ_RUNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        OWNER_GEN.load(Ordering::SeqCst),
        1,
        "owner got generation 1"
    );
    assert_eq!(
        READ_SUM.load(Ordering::SeqCst),
        2,
        "both readers copied the generation-1 handle"
    );
    assert!(
        GADGET.take().is_none(),
        "consume slot is empty while the owner holds the value"
    );
    assert!(
        HANDLE.get().is_some(),
        "shared slot stays filled after both reads"
    );

    sup.teardown().await.expect("teardown");
    assert_eq!(
        DROPPED.load(Ordering::SeqCst),
        1,
        "owner dropped its Gadget"
    );
    assert!(
        GADGET.take().is_none(),
        "consume slot still empty after drop"
    );
    assert!(
        HANDLE.get().is_none(),
        "the provider cleared its fan-out slot before acking"
    );

    sup.respawn_terminate(&spawner).await.expect("respawn");
    settle(|| READ_RUNS.load(Ordering::SeqCst) == 4).await;
    assert_eq!(
        OWNER_GEN.load(Ordering::SeqCst),
        2,
        "owner got the REBUILT Gadget"
    );
    assert_eq!(
        READ_SUM.load(Ordering::SeqCst),
        6,
        "readers got the generation-2 handle (2 + 4)"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn provider_node_builds_and_rebuilds_consumer_resources() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    // Tick mock time: the provider's build delay and the consumers' bounded
    // gate waits are real timers against the mock clock.
    let deadline = StdInstant::now() + StdDuration::from_secs(20);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (gen={}, owner={}, runs={}, sum={}, dropped={})",
            GENERATION.load(Ordering::SeqCst),
            OWNER_GEN.load(Ordering::SeqCst),
            READ_RUNS.load(Ordering::SeqCst),
            READ_SUM.load(Ordering::SeqCst),
            DROPPED.load(Ordering::SeqCst),
        );
        clock.advance(Duration::from_millis(10));
        std::thread::sleep(StdDuration::from_millis(1));
    }
}
