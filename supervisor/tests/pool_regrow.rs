//! An elastic pool must regrow on its own once its provider recovers: the
//! readiness poke (`set_ready` re-evaluates scaling), the
//! `restart` cycle (whose up-wave deliberately excludes OnDemand members),
//! and `apply_bind`'s OnDemand recovery arm. Before these pokes existed the
//! pool driver parked on `wait_scale` with a declined Grow and nobody left
//! to wake it. The plain-dep pool in the second half has no `ready` marker
//! on its provider, so its regrow can only ride on restart's own poke.
//!
//! The script never calls `request_scale` itself, and every settle waits on
//! worker-side counters (a member's `is_running` flips inside the driver's
//! own poll, before the member task has run — racing an assert against it
//! reads a half-spawned world).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph};

static LINK_SPAWNS: AtomicU32 = AtomicU32::new(0);
static POOL_SPAWNS: AtomicU32 = AtomicU32::new(0);
static PLAIN_SPAWNS: AtomicU32 = AtomicU32::new(0);
static CREW_SPAWNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn link_worker(node: &'static TaskNode) {
    LINK_SPAWNS.fetch_add(1, Ordering::SeqCst);
    node.set_ready();
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn session_worker(node: &'static TaskNode) {
    POOL_SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

// One task fn per spawn site: the generated shell's pool_size is 1, so the
// provider and the pool members cannot share a worker.
async fn plain_worker(node: &'static TaskNode) {
    PLAIN_SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn crew_worker(node: &'static TaskNode) {
    CREW_SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    node LINK = Terminate, deps: [], task: link_worker;
    pool SESSIONS = [OnDemand, OnDemand], deps: [LINK ready bound],
        task: session_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: 0, max: 2;
    node PLAIN = Terminate, deps: [], task: plain_worker;
    pool CREW = [OnDemand, OnDemand], deps: [PLAIN],
        task: crew_worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: 0, max: 2;
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

fn pool_spawns() -> u32 {
    POOL_SPAWNS.load(Ordering::SeqCst)
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    let script = async {
        // Let the pool driver take its first pass with LINK down: the Grow is
        // declined at the deps gate and the driver parks on `wait_scale`.
        for _ in 0..50 {
            embassy_futures::yield_now().await;
        }
        assert_eq!(pool_spawns(), 0, "no member before the provider");

        // Bring-up: the freshly asserted readiness is the only wake signal
        // the parked driver gets, and it must be enough.
        sup.start(&spawner).await.expect("bring-up");
        settle(|| pool_spawns() == 1 && SESSIONS[0].is_running()).await;
        assert!(
            pool_spawns() == 1 && SESSIONS[0].is_running(),
            "the pool grows once LINK asserts readiness"
        );

        // Restart: the down-wave stops the member, the up-wave excludes
        // OnDemand — the respawned LINK's set_ready must regrow the pool.
        sup.restart(&LINK, &spawner).await.expect("restart");
        settle(|| LINK_SPAWNS.load(Ordering::SeqCst) == 2).await;
        settle(|| pool_spawns() == 2 && SESSIONS[0].is_running()).await;
        assert!(
            pool_spawns() == 2 && SESSIONS[0].is_running(),
            "the pool regrows after a restart of its provider"
        );

        // Readiness flap over the bound dep: the member bound-stops, and
        // recovery hands it back to the policy with a poke.
        LINK.clear_ready();
        sup.apply_bind(&spawner).await.expect("bound stop");
        settle(|| !SESSIONS[0].is_running()).await;
        assert!(
            SESSIONS[0].is_bound_stopped(),
            "the member follows the withdrawn readiness"
        );
        LINK.set_ready();
        sup.apply_bind(&spawner).await.expect("bound recovery");
        settle(|| pool_spawns() == 3 && SESSIONS[0].is_running()).await;
        assert!(
            pool_spawns() == 3 && SESSIONS[0].is_running(),
            "the pool regrows after the bound provider recovers"
        );

        // Plain-dep pool (no `ready` marker on the provider): the readiness
        // poke can never fire for it, so the regrow after a restart rides on
        // restart's own poke alone. PLAIN came up with the boot `start()`
        // above and the awake driver grew CREW at boot; the interesting half
        // is the restart.
        assert!(PLAIN.is_running(), "PLAIN came up at boot");
        settle(|| CREW_SPAWNS.load(Ordering::SeqCst) == 1 && CREW[0].is_running()).await;
        assert!(
            CREW_SPAWNS.load(Ordering::SeqCst) == 1 && CREW[0].is_running(),
            "the plain-dep pool grew at boot"
        );
        sup.restart(&PLAIN, &spawner)
            .await
            .expect("restart plain provider");
        settle(|| PLAIN_SPAWNS.load(Ordering::SeqCst) == 2).await;
        settle(|| CREW_SPAWNS.load(Ordering::SeqCst) == 2 && CREW[0].is_running()).await;
        assert!(
            CREW_SPAWNS.load(Ordering::SeqCst) == 2 && CREW[0].is_running(),
            "the plain-dep pool regrows after a restart of its provider"
        );

        DONE.store(true, Ordering::SeqCst);
    };
    if let Either::First(err) = select(sup.run_pools(&spawner), script).await {
        panic!("pool driver faulted on {}", err.node.name());
    }
}

#[test]
fn pool_regrows_without_manual_pokes() {
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
            "did not complete (link spawns={}, pool spawns={}, crew spawns={}, member running={})",
            LINK_SPAWNS.load(Ordering::SeqCst),
            POOL_SPAWNS.load(Ordering::SeqCst),
            CREW_SPAWNS.load(Ordering::SeqCst),
            SESSIONS[0].is_running(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
