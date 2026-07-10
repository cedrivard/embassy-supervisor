//! Behavioral test for the **provider-node pattern** (the graph-native home for
//! `hw_init`-style async multi-output construction): a first-in-topo node whose
//! worker BUILDS the resources at runtime and `provide()`s them into other
//! nodes' slots — one `consume` slot (an owned driver, dropped at teardown,
//! rebuilt each cycle) and one `shared` slot (a `Copy` handle fanned out to two
//! consumers). The consumers carry `slot_timeout:` sized to the provider's
//! build time; the supervisor's gate-await turns the whole thing into a clean
//! rendezvous:
//!
//! * cold `start()`: consumers' gate waits park until the provider's first poll
//!   finishes building (a mock 500 ms — past the 100 ms default that would have
//!   failed `Busy`, inside the declared 5 s budget);
//! * `teardown()` → `respawn_terminate()`: the provider re-runs FIRST (topo
//!   order), re-provides fresh values (generation 2), and the consumers re-take
//!   them — the `consume` slot empty in between, the `shared` slot still
//!   holding generation 1 until the provider overwrites it.
//!
//! Mock TIME must advance (the provider's build delay + the gate waits are real
//! timers), so the test thread ticks `MockDriver` throughout.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver, Timer};

/// The consume-kind product: owned by ONE node, dropped at its teardown
/// (observable through `Drop`), rebuilt by the provider each cycle. The
/// generation stamp proves the respawn got the provider's FRESH build.
struct Gadget {
    generation: u32,
}

impl Drop for Gadget {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

/// The shared-kind product: a `Copy` handle fanned out to two reader nodes.
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

/// The provider: async build (mock 500 ms — a radio bring-up stand-in), then
/// `provide()` everything and park supervised. It HOLDS nothing afterwards, so
/// its own teardown is a bare ack; a Terminate respawn re-runs the build.
async fn provider_worker(node: &'static TaskNode) {
    Timer::after_millis(500).await;
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    GADGET.provide(Gadget { generation });
    HANDLE.provide(Handle { generation });
    node.wait_shutdown().await;
    node.ack_dropped();
}

/// `consume` consumer: owns the Gadget; returning after the ack drops it.
async fn owner_worker(node: &'static TaskNode, gadget: Gadget) {
    OWNER_GEN.store(gadget.generation, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
}

/// `shared` consumer: its own copy of the fan-out handle (slot stays filled).
async fn reader_worker(node: &'static TaskNode, handle: Handle) {
    READ_SUM.fetch_add(handle.generation, Ordering::SeqCst);
    READ_RUNS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
}

supervisor_graph! {
    // First in topo order; the consumers' `deps:` guarantee it (re)spawns
    // before them, and their gate waits cover its build time.
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

    // NOTHING is provided up front — the provider node does it at runtime.
    // start() parks on the consumers' gates until the provider's first poll
    // builds and provides (mock 500 ms < their 5 s slot_timeout).
    sup.start(spawner).await.expect("start");
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

    sup.teardown().await;
    assert_eq!(
        DROPPED.load(Ordering::SeqCst),
        1,
        "owner dropped its Gadget"
    );
    assert!(
        GADGET.take().is_none(),
        "consume slot still empty after drop"
    );

    // Respawn: the provider re-runs first (topo), rebuilds, re-provides
    // generation 2; the consumers' gate waits rendezvous with the fresh values.
    sup.respawn_terminate(spawner).await.expect("respawn");
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
