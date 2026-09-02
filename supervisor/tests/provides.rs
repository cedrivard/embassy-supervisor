use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

#[derive(Clone, Copy)]
struct Handle {
    generation: u32,
}

static GENERATION: AtomicU32 = AtomicU32::new(0);
static READ_SUM: AtomicU32 = AtomicU32::new(0);
static READ_RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn provider_worker(node: &'static TaskNode) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    HANDLE.provide(Handle { generation });
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

async fn pauser_worker(node: &'static TaskNode) {
    PHANDLE.provide(Handle { generation: 1 });
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
    }
}

async fn preader_worker(node: &'static TaskNode, _handle: Handle) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node PROVIDER = Terminate, deps: [], task: provider_worker,
        provides: [HANDLE];
    // `serialized`: both holders run on the supervisor's executor, which is
    // all the marker asks (it changes nothing at runtime).
    node READ_A = Terminate, deps: [PROVIDER], task: reader_worker,
        slot_timeout: 5000,
        resources: [HANDLE: shared serialized Handle];
    node READ_B = Terminate, deps: [PROVIDER], task: reader_worker,
        slot_timeout: 5000,
        resources: [HANDLE: shared serialized Handle];

    node PAUSER = Pause, deps: [], task: pauser_worker,
        provides: [PHANDLE];
    node READ_P = Terminate, deps: [PAUSER], task: preader_worker,
        slot_timeout: 5000,
        resources: [PHANDLE: shared Handle];
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
        READ_SUM.load(Ordering::SeqCst),
        2,
        "both readers copied the generation-1 handle"
    );
    assert!(HANDLE.get().is_some(), "shared slot filled while up");
    assert!(PHANDLE.get().is_some());

    sup.teardown().await.expect("teardown");
    assert!(
        HANDLE.get().is_none(),
        "the Terminate provider's ack cleared what it provides"
    );
    assert!(
        PHANDLE.get().is_some(),
        "a Pause ack is a park: the parked task still backs its value"
    );

    sup.respawn_terminate(&spawner).await.expect("respawn");
    settle(|| READ_RUNS.load(Ordering::SeqCst) == 4).await;
    assert_eq!(
        READ_SUM.load(Ordering::SeqCst),
        6,
        "the respawned readers got the generation-2 handle (2 + 4)"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_providers_ack_clears_what_it_fills() {
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
            "did not complete (gen={}, runs={}, sum={})",
            GENERATION.load(Ordering::SeqCst),
            READ_RUNS.load(Ordering::SeqCst),
            READ_SUM.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(1));
    }
}
