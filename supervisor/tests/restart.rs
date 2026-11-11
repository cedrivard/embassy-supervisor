mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

supervisor_graph! {
    node ROOT = Terminate, deps: [], task: plain;

    node PROVIDER = Terminate, deps: [ROOT], task: provider;

    node CONSUMER = Terminate, deps: [PROVIDER ready], task: plain, slot_timeout: 5000;
    node DOWNSTREAM = Terminate, deps: [CONSUMER], task: plain;

    node SIBLING = Terminate, deps: [ROOT], task: plain, disabled;
    node HELD_DOWN = Terminate, deps: [PROVIDER], task: plain, disabled;

    node SIB = Terminate, deps: [ROOT], task: plain, disabled;
    node OD = OnDemand, deps: [ROOT], task: plain;

    node RADIO = Terminate, deps: [], task: radio;
    node LINK = Pause, deps: [RADIO ready], task: link;
}

static PROVIDER_GEN: AtomicU32 = AtomicU32::new(0);
static WITHHOLD_READY: AtomicBool = AtomicBool::new(false);
static CONSUMER_STARTS: AtomicU32 = AtomicU32::new(0);
static DOWNSTREAM_STARTS: AtomicU32 = AtomicU32::new(0);
static ROOT_STARTS: AtomicU32 = AtomicU32::new(0);
static SIB_STARTS: AtomicU32 = AtomicU32::new(0);
static OD_STARTS: AtomicU32 = AtomicU32::new(0);
static RADIO_RUNS: AtomicU32 = AtomicU32::new(0);
/// Cleared, RADIO's next activation withholds readiness until it is set again.
static RADIO_GO: AtomicBool = AtomicBool::new(true);
static LINK_SPAWNS: AtomicU32 = AtomicU32::new(0);
static LINK_RESUMES: AtomicU32 = AtomicU32::new(0);
/// Set for the last phase, the only one that runs a gate to its timeout.
static ADVANCE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn plain(node: &'static TaskNode) {
    if core::ptr::eq(node, &CONSUMER) {
        CONSUMER_STARTS.fetch_add(1, Ordering::SeqCst);
    }
    if core::ptr::eq(node, &DOWNSTREAM) {
        DOWNSTREAM_STARTS.fetch_add(1, Ordering::SeqCst);
    }
    if core::ptr::eq(node, &ROOT) {
        ROOT_STARTS.fetch_add(1, Ordering::SeqCst);
    }
    if core::ptr::eq(node, &SIB) {
        SIB_STARTS.fetch_add(1, Ordering::SeqCst);
    }
    if core::ptr::eq(node, &OD) {
        OD_STARTS.fetch_add(1, Ordering::SeqCst);
    }
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn provider(node: &'static TaskNode) {
    PROVIDER_GEN.fetch_add(1, Ordering::SeqCst);
    if !WITHHOLD_READY.load(Ordering::SeqCst) {
        node.set_ready();
    }
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn radio(node: &'static TaskNode) {
    RADIO_RUNS.fetch_add(1, Ordering::SeqCst);
    while !RADIO_GO.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    node.set_ready();
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn link(node: &'static TaskNode) {
    LINK_SPAWNS.fetch_add(1, Ordering::SeqCst);
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
        LINK_RESUMES.fetch_add(1, Ordering::SeqCst);
    }
}

async fn settle() {
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    sup.start(&spawner).await.expect("start");
    settle().await;
    assert_eq!(PROVIDER_GEN.load(Ordering::SeqCst), 1);
    assert_eq!(CONSUMER_STARTS.load(Ordering::SeqCst), 1);
    assert_eq!(ROOT_STARTS.load(Ordering::SeqCst), 1);
    assert!(!SIBLING.is_running(), "declared `disabled`");

    sup.restart(&PROVIDER, &spawner).await.expect("restart");
    settle().await;

    assert_eq!(
        PROVIDER_GEN.load(Ordering::SeqCst),
        2,
        "the target was respawned"
    );
    assert_eq!(
        CONSUMER_STARTS.load(Ordering::SeqCst),
        2,
        "its dependent was cycled with it — the re-gate a bare stop/start misses"
    );
    assert!(DOWNSTREAM.is_running(), "the transitive dependent is back");
    assert_eq!(
        DOWNSTREAM_STARTS.load(Ordering::SeqCst),
        2,
        "back because the cascade cycled the second hop, not because it was \
         never stopped"
    );

    assert_eq!(
        ROOT_STARTS.load(Ordering::SeqCst),
        1,
        "a dependency of the target is not cycled"
    );
    assert!(
        !SIBLING.is_running() && SIBLING.is_disabled(),
        "a deliberately disabled node is not resurrected by an unrelated restart"
    );
    assert!(
        !HELD_DOWN.is_running() && HELD_DOWN.is_disabled(),
        "a disabled dependent inside the restart set is not brought back up"
    );

    assert!(
        !PROVIDER.is_disabled() && !CONSUMER.is_disabled(),
        "a cycle is not a stop; nothing in the set is left marked disabled"
    );

    // A disabled node started by hand is running, so a restart brings it back:
    // the cycle restores what was running, latch and all.
    sup.start_node(&SIB, &spawner)
        .await
        .expect("an explicit start overrides the latch");
    settle().await;
    assert!(SIB.is_running() && SIB.is_disabled());
    sup.restart(&SIB, &spawner).await.expect("restart");
    settle().await;
    assert_eq!(SIB_STARTS.load(Ordering::SeqCst), 2, "SIB was cycled");
    assert!(
        SIB.is_running() && SIB.is_disabled(),
        "a running disabled node comes back, still latched"
    );

    // A running OnDemand node is not left down either.
    sup.start_node(&OD, &spawner)
        .await
        .expect("start on demand");
    settle().await;
    assert!(OD.is_running());
    sup.restart(&OD, &spawner).await.expect("restart");
    settle().await;
    assert_eq!(OD_STARTS.load(Ordering::SeqCst), 2, "OD was cycled");
    assert!(OD.is_running(), "a running OnDemand node comes back");

    // A parked Pause dependent is resumed only once its `ready` dep asserts:
    // the clock stands still, so LINK's gate budget never runs out here, and
    // the restart can only end after the script lets RADIO assert.
    assert!(LINK.is_running() && RADIO.is_ready());
    RADIO_GO.store(false, Ordering::SeqCst);
    let script = async {
        for _ in 0..200 {
            embassy_futures::yield_now().await;
        }
        assert_eq!(RADIO_RUNS.load(Ordering::SeqCst), 2, "RADIO is back");
        assert!(
            !LINK.is_running(),
            "LINK stays parked while RADIO withholds readiness"
        );
        RADIO_GO.store(true, Ordering::SeqCst);
        core::future::pending::<()>().await;
    };
    match select(sup.restart(&RADIO, &spawner), script).await {
        Either::First(r) => {
            r.expect("restart");
            assert!(
                RADIO_GO.load(Ordering::SeqCst),
                "the restart ended before RADIO was let assert readiness"
            );
        }
        Either::Second(()) => unreachable!("the script never ends"),
    }
    settle().await;
    assert!(
        LINK.is_running(),
        "LINK resumed once RADIO asserted readiness"
    );
    assert_eq!(
        (
            LINK_SPAWNS.load(Ordering::SeqCst),
            LINK_RESUMES.load(Ordering::SeqCst)
        ),
        (1, 1),
        "resumed in place, never respawned"
    );

    ADVANCE.store(true, Ordering::SeqCst);
    WITHHOLD_READY.store(true, Ordering::SeqCst);
    let before = CONSUMER_STARTS.load(Ordering::SeqCst);
    let err = sup
        .restart(&PROVIDER, &spawner)
        .await
        .expect_err("the consumer's ready gate must not pass");
    settle().await;
    assert!(
        matches!(
            err.kind,
            embassy_supervisor::FaultKind::ReadyDepTimeout { .. }
        ),
        "the gate that failed is named, not a generic spawn error: {err}"
    );
    assert_eq!(err.node.name(), "consumer", "the error names what failed");
    assert_eq!(
        CONSUMER_STARTS.load(Ordering::SeqCst),
        before,
        "the dependent did not start against an unready provider"
    );
    assert_eq!(
        PROVIDER_GEN.load(Ordering::SeqCst),
        3,
        "the target itself did come back"
    );

    DONE.store(true, Ordering::SeqCst);
}

/// Moves the mock clock 5 ms per executor pass while `ADVANCE` is set, so
/// device time passes only while the executor runs: a descheduled executor
/// thread cannot overshoot a gate budget.
#[embassy_executor::task]
async fn clock_ticker() {
    let clock = MockDriver::get();
    while !DONE.load(Ordering::SeqCst) {
        if ADVANCE.load(Ordering::SeqCst) {
            clock.advance(embassy_time::Duration::from_millis(5));
        }
        embassy_futures::yield_now().await;
    }
}

#[test]
fn rest_for_one_cycles_downstream_only() {
    support::run(
        |spawner| {
            spawner.spawn(driver(spawner).unwrap());
            spawner.spawn(clock_ticker().unwrap());
        },
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
}
