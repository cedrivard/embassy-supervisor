mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use support::settle;

#[derive(Clone, Copy)]
struct Handle {
    generation: u32,
}

static GENERATION: AtomicU32 = AtomicU32::new(0);
static READ_SUM: AtomicU32 = AtomicU32::new(0);
static READ_RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);
static PEXIT_QUIT: Signal<CriticalSectionRawMutex, ()> = Signal::new();

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

async fn pexit_worker(_node: &'static TaskNode) {
    XHANDLE.provide(Handle { generation: 1 });
    PEXIT_QUIT.wait().await;
}

async fn gprovider_worker(node: &'static TaskNode) {
    GHANDLE.provide(Handle { generation: 1 });
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn gfirst_worker(node: &'static TaskNode) {
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

    node PEXIT = Pause, deps: [], task: pexit_worker, provides: [XHANDLE];
    node READ_X = Terminate, deps: [PEXIT], task: preader_worker, disabled,
        resources: [XHANDLE: shared Handle];

    // `GHANDLE`'s first declarer is gated off and a later one is not, so the
    // slot exists and the provider's ack must clear it. `GBUF`'s only
    // declarer is compiled out (its task fn need not exist), so the slot and
    // the provider's entry for it are gone too.
    node G_PROVIDER = Terminate, deps: [], task: gprovider_worker, provides: [GHANDLE, GBUF];
    node G_FIRST = Terminate, deps: [G_PROVIDER], task: gfirst_worker,
        resources: [#[cfg(any())] GHANDLE: shared Handle];
    node G_SECOND = Terminate, deps: [G_PROVIDER], task: preader_worker,
        slot_timeout: 5000, resources: [GHANDLE: shared Handle];
    #[cfg(any())]
    node GONE = Terminate, deps: [], task: gone_worker, resources: [GBUF: u32];
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start");
    settle("second read run", || READ_RUNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        READ_SUM.load(Ordering::SeqCst),
        2,
        "both readers copied the generation-1 handle"
    );
    assert!(HANDLE.get().is_some(), "shared slot filled while up");
    assert!(PHANDLE.get().is_some());
    assert!(GHANDLE.get().is_some());

    settle("XHANDLE published", || XHANDLE.get().is_some()).await;
    assert!(
        XHANDLE.get().is_some(),
        "the Pause provider published its value"
    );
    PEXIT_QUIT.signal(());
    settle("PEXIT exited", || PEXIT.has_exited()).await;
    assert!(PEXIT.has_exited() && !PEXIT.is_running());
    assert!(
        XHANDLE.get().is_none(),
        "a Pause node that exits on its own clears what it provides"
    );

    sup.teardown().await.expect("teardown");
    assert!(
        HANDLE.get().is_none(),
        "the Terminate provider's ack cleared what it provides"
    );
    assert!(
        PHANDLE.get().is_some(),
        "a Pause ack is a park: the parked task still backs its value"
    );
    assert!(
        GHANDLE.get().is_none(),
        "the provider's ack clears a slot whose first declarer is gated off"
    );

    sup.respawn_terminate(&spawner).await.expect("respawn");
    settle("fourth read run", || READ_RUNS.load(Ordering::SeqCst) == 4).await;
    assert_eq!(
        READ_SUM.load(Ordering::SeqCst),
        6,
        "the respawned readers got the generation-2 handle (2 + 4)"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_providers_ack_clears_what_it_fills() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (gen={}, runs={}, sum={})",
                GENERATION.load(Ordering::SeqCst),
                READ_RUNS.load(Ordering::SeqCst),
                READ_SUM.load(Ordering::SeqCst),
            )
        },
    );
}
