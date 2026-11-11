//! A hold latched by `deactivate` while a start is in flight must survive the
//! spawn. The interleave, made deterministic by the ready-dep gate: a
//! bound-stopped member's provider is still un-ready, so a recovery
//! `start_node` parks inside `await_ready_deps` — one long await between the
//! caller's guard checks and the spawn's flag clear. A concurrent
//! `deactivate` of the provider runs in that window and latches `collateral`
//! on the not-yet-running member without stopping it; the member's own
//! `set_ready` then releases the parked gate and the spawn completes. The
//! member must run *flagged*: the manual-start override clears the hold at
//! spawn ENTRY, so a mid-flight latch is the last word. (An earlier version
//! cleared the hold at spawn completion; this test reproduces the
//! silently-unheld running member that produced.)

mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph};
use support::settle;

static LINK_SPAWNS: AtomicU32 = AtomicU32::new(0);
static CREW_SPAWNS: AtomicU32 = AtomicU32::new(0);
static UNHELD_RUNNING: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn link_worker(node: &'static TaskNode) {
    LINK_SPAWNS.fetch_add(1, Ordering::SeqCst);
    node.set_ready();
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn crew_worker(node: &'static TaskNode) {
    let n = CREW_SPAWNS.fetch_add(1, Ordering::SeqCst) + 1;
    // First poll of any instance after the boot grow (#1): if a deactivate
    // latched the hold while this spawn was in flight, the flag must still
    // be set when the task runs.
    if n >= 2 && node.is_running() && !node.is_collateral() && !node.is_disabled() {
        UNHELD_RUNNING.store(true, Ordering::SeqCst);
    }
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node LINK = Terminate, deps: [], task: link_worker;
    pool CREW = [OnDemand, OnDemand], deps: [LINK ready bound],
        task: crew_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: 0, max: 2;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    // Boot, grow a member by hand, then bound-stop it through a readiness flap.
    sup.start(&spawner).await.expect("bring-up");
    sup.start_node(&CREW[0], &spawner).await.expect("grow");
    settle("first crew spawn", || {
        CREW_SPAWNS.load(Ordering::SeqCst) == 1
    })
    .await;

    LINK.clear_ready();
    sup.apply_bind(&spawner).await.expect("bound stop");
    settle("member bound-stopped", || !CREW[0].is_running()).await;
    assert!(CREW[0].is_bound_stopped(), "the member bound-stopped");

    // The provider is still un-ready, so the recovery start_node parks inside
    // await_ready_deps. Joined alongside it: deactivate latches the hold
    // (member not running, so it is not stopped either), then set_ready
    // releases the parked gate and the in-flight spawn completes — by which
    // point LINK is deactivated and the member is collateral.
    let recovery = async {
        sup.start_node(&CREW[0], &spawner)
            .await
            .expect("the parked recovery start completes once LINK re-asserts");
    };
    let deactivate_mid_gate = async {
        // Let start_node reach its gate wait first.
        embassy_futures::yield_now().await;
        sup.deactivate(&LINK).await.expect("deactivate mid-spawn");
        assert!(CREW[0].is_collateral(), "the hold latched mid-spawn");
        LINK.set_ready();
    };
    embassy_futures::join::join(recovery, deactivate_mid_gate).await;

    // The clock never moves, so the parked gate resolves only through LINK's
    // set_ready: the recovery spawn completes and its instance runs flagged.
    settle("second crew spawn", || {
        CREW_SPAWNS.load(Ordering::SeqCst) == 2
    })
    .await;
    assert!(
        CREW[0].is_running() && CREW[0].is_collateral(),
        "the member runs, still held by the mid-flight deactivate"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn mid_flight_hold_survives_the_spawn() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (link spawns={}, crew spawns={}, member running={})",
                LINK_SPAWNS.load(Ordering::SeqCst),
                CREW_SPAWNS.load(Ordering::SeqCst),
                CREW[0].is_running(),
            )
        },
    );

    assert!(LINK.is_disabled(), "the provider keeps its own latch");
    assert!(
        !UNHELD_RUNNING.load(Ordering::SeqCst),
        "a member respawned across the deactivate never ran unheld \
         (crew spawns={}, member running={}, collateral={})",
        CREW_SPAWNS.load(Ordering::SeqCst),
        CREW[0].is_running(),
        CREW[0].is_collateral(),
    );
}
