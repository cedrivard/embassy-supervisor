//! `shared local` resources on a `pool`: the one pool kind that combines a
//! `!Send` payload with fan-out — the `embassy_net::Stack` shape (a `Copy`
//! handle that is not `Send`, here a `&'static Cell<u32>`). The pool rides the
//! pool-wide shared-slot path exactly like nodes do: the pre-spawn gate waits
//! for `provide()` (an unprovided floor fail-closes `start()` with
//! `SpawnError::Busy`), every member's glue copies the SAME handle out with
//! `get()` (the slot stays filled), and a member exit neither empties nor
//! re-provides it — the slot is never taken, so there is no lend to give back,
//! and a re-grown member fans out again without a re-provide.
//! (Take-kind `local` on pools stays rejected — per-member restore is
//! deferred; that rejection is locked by the macro UI tests.) Harness as
//! resource_kinds.rs: the Busy path needs mock time ticking.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::{SpawnError, Spawner};
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver};

/// The fanned-out handle: `Copy` (shared) AND `!Send`/`!Sync` (the
/// `PhantomData` raw pointer kills the auto impls, like the `&RefCell`
/// inside `embassy_net::Stack`) — the one pool kind needing `shared local`.
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
    node.mark_busy(); // saturate the floor so the policy wants member 1
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

/// Drive the pool until `until()` holds (or the yield budget elapses); see
/// pool_member_resources.rs for why all pool driving stays in this one task.
async fn drive_until<const N: usize>(
    sup: &Supervisor<N>,
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
    };
    match select(sup.run_pools(spawner), watch).await {
        Either::First(err) => panic!("pool driver errored: {:?}", err.node.name),
        Either::Second(()) => {}
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    // Gate: with the pool-wide slot empty, the floor member's pre-spawn wait
    // times out (mock clock is ticking) and start() fail-closes — the shared
    // slot is a ResourceGate on the graph-site local type, same as on nodes.
    assert!(
        matches!(sup.start(spawner).await, Err(SpawnError::Busy)),
        "unprovided shared local slot must fail start() with Busy"
    );
    assert_eq!(
        RUNS.load(Ordering::SeqCst),
        0,
        "nothing spawned unprovisioned"
    );

    // Provide and start: the floor member copies the handle out.
    HANDLE.provide(Handle {
        hits: &BACKING,
        _local_only: core::marker::PhantomData,
    });
    sup.start(spawner).await.expect("start with provided slot");
    settle(|| RUNS.load(Ordering::SeqCst) == 1).await;

    // Fan-out is non-destructive: the slot is STILL FILLED after the spawn,
    // so growth needs no re-provide.
    assert!(
        HANDLE.get().is_some(),
        "shared slot stays filled after the floor spawns"
    );

    // Grow to member 1 (floor is busy): it copies the SAME handle — the cell
    // it observes is already bumped by member 0.
    embassy_supervisor::request_scale();
    drive_until(&sup, spawner, || RUNS.load(Ordering::SeqCst) == 2).await;
    let first = SEEN[0].load(Ordering::SeqCst);
    let second = SEEN[1].load(Ordering::SeqCst);
    assert_eq!((first, second), (1, 2), "both members share ONE handle");

    // Nothing is restored on member exit: a shared slot is never emptied, so
    // there is no lend to hand back. Stop the grown member and the slot still
    // holds the same handle, its backing untouched.
    sup.stop_node(&FAN[1]).await.expect("stop the grown member");
    settle(|| !FAN[1].is_running()).await;
    assert!(HANDLE.get().is_some(), "the slot survives a member exit");
    assert_eq!(
        BACKING.load(Ordering::SeqCst),
        2,
        "the exit path touched the fanned-out value"
    );

    // So a re-grown member needs no re-provide: it copies the live handle out
    // and keeps counting on the same backing cell.
    sup.start_node(&FAN[1], spawner).await.expect("re-grow");
    settle(|| RUNS.load(Ordering::SeqCst) == 3).await;
    assert_eq!(
        SEEN[1].load(Ordering::SeqCst),
        3,
        "the respawned member fanned out from the same slot"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn pool_shared_local_resource() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    // Tick mock time so the unprovided floor's gate timeout can expire (the
    // other waits resolve by signal and never park).
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (runs={}, seen=[{},{}])",
            RUNS.load(Ordering::SeqCst),
            SEEN[0].load(Ordering::SeqCst),
            SEEN[1].load(Ordering::SeqCst),
        );
        clock.advance(Duration::from_millis(10));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
