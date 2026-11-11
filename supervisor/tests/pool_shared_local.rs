mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{DeferredShrink, FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_time::Duration;
use support::settle;

#[derive(Clone, Copy)]
struct Handle {
    hits: &'static AtomicU32,
    _local_only: core::marker::PhantomData<*const ()>,
}

static BACKING: AtomicU32 = AtomicU32::new(0);

supervisor_graph! {
    pool FAN = [Terminate, OnDemand], deps: [],
        task: fan_worker,
        resources: [HANDLE: shared local Handle],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
}

/// The value each member observed at entry: member `I` bumps the shared cell
/// and records what it saw. If both members really hold the SAME handle, the
/// second observation is exactly one higher than the first.
static SEEN: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn fan_worker(node: &'static TaskNode, handle: Handle) {
    let i = FAN_POOL.member_index(node).expect("a member");
    let seen = handle.hits.fetch_add(1, Ordering::SeqCst) + 1;
    SEEN[i].store(seen, Ordering::SeqCst);
    RUNS.fetch_add(1, Ordering::SeqCst);
    node.mark_busy();
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn drive_until<const N: usize, T: embassy_supervisor::Topology<N>>(
    sup: &Supervisor<N, T>,
    spawner: Spawner,
    mut until: impl FnMut() -> bool,
) {
    use embassy_futures::select::{Either, select};
    let watch = async {
        for _ in 0..10_000 {
            if until() {
                return;
            }
            embassy_futures::yield_now().await;
        }
        assert!(until(), "drive_until: did not settle");
    };
    match select(sup.run_pools(&spawner), watch).await {
        Either::First(err) => panic!("pool driver errored: {:?}", err.node.name()),
        Either::Second(()) => {}
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    let err = sup
        .start(&spawner)
        .await
        .expect_err("unprovided shared local slot must fail start()");
    assert!(
        matches!(err.kind, FaultKind::ResourceMissing),
        "and must name the missing slot rather than a task pool: {err}"
    );
    assert_eq!(
        RUNS.load(Ordering::SeqCst),
        0,
        "nothing spawned unprovisioned"
    );

    // SAFETY: called on the slot's executor: the pool's members run on this
    // one.
    unsafe {
        HANDLE.provide(Handle {
            hits: &BACKING,
            _local_only: core::marker::PhantomData,
        })
    };
    sup.start(&spawner).await.expect("start with provided slot");
    settle("first run", || RUNS.load(Ordering::SeqCst) == 1).await;

    assert!(
        HANDLE.is_filled(),
        "shared slot stays filled after the floor spawns"
    );

    embassy_supervisor::request_scale();
    drive_until(&sup, spawner, || RUNS.load(Ordering::SeqCst) == 2).await;
    let first = SEEN[0].load(Ordering::SeqCst);
    let second = SEEN[1].load(Ordering::SeqCst);
    assert_eq!((first, second), (1, 2), "both members share ONE handle");

    sup.stop_node(&FAN[1]).await.expect("stop the grown member");
    settle("FAN[1] stopped", || !FAN[1].is_running()).await;
    assert!(HANDLE.is_filled(), "the slot survives a member exit");
    assert_eq!(
        BACKING.load(Ordering::SeqCst),
        2,
        "the exit path left the fanned-out value alone: two entry bumps, nothing else"
    );

    sup.start_node(&FAN[1], &spawner).await.expect("re-grow");
    settle("third run", || RUNS.load(Ordering::SeqCst) == 3).await;
    assert_eq!(
        SEEN[1].load(Ordering::SeqCst),
        3,
        "the respawned member fanned out from the same slot"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn pool_shared_local_resource() {
    // Tick mock time so the unprovided floor's gate timeout can expire (the
    // other waits resolve by signal and never park).
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Free(Duration::from_millis(10)),
        10,
        || {
            format!(
                "did not complete (runs={}, seen=[{},{}])",
                RUNS.load(Ordering::SeqCst),
                SEEN[0].load(Ordering::SeqCst),
                SEEN[1].load(Ordering::SeqCst),
            )
        },
    );
}
