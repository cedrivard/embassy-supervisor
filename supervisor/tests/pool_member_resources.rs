//! Per-member pool resources: take-kind entries become per-member slot
//! arrays — member `I` takes/restores element `I` exclusively, so the floor
//! comes up with only floor-many elements provided, growth waits for the burst
//! member's own element, restore lands back on the same index across a
//! stop/regrow, and `member_index` gives a worker its position. Also covers
//! const-expr `min:`/`max:` (the emitted `_MIN`/`_MAX` consts are the source of
//! truth). Harness as teardown.rs.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph};

/// Const-driven bounds: the macro can't parse-time-validate these, so the
/// emitted consts + const asserts carry the checks.
const FLOOR: usize = 1;

/// A per-member connection resource: distinct per member, non-Copy.
struct Conn {
    id: u32,
    uses: u32,
}

supervisor_graph! {
    pool WORKERS = [Terminate, OnDemand], deps: [],
        task: worker,
        resources: [CONN: Conn],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: FLOOR, max: FLOOR + 1;
}

static SPAWNS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static SEEN_ID: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static DONE: AtomicBool = AtomicBool::new(false);

/// Lend-kind worker: receives its member's own `Conn` as `&mut`, records which
/// instance it got, and parks; the shell restores to the same array element on
/// exit.
async fn worker(node: &'static TaskNode, conn: &mut Conn) {
    let i = WORKERS_POOL.member_index(node).expect("a member");
    SPAWNS[i].fetch_add(1, Ordering::SeqCst);
    SEEN_ID[i].store(conn.id, Ordering::SeqCst);
    conn.uses += 1;
    node.mark_busy(); // keep the policy wanting to grow
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

/// Drive the pool until `until()` holds (or a generous yield budget elapses).
/// Dropping `run_pools` mid-flight is documented-safe (a half-applied action is
/// re-driven), and keeping ALL pool driving inside the one driver task is the
/// production shape — a concurrent pool task would race the manual
/// stop/start below on the member's handshake flags.
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
        Either::First(err) => panic!("pool shrink timed out: {:?}", err.node.name),
        Either::Second(()) => {}
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    // Const-expr bounds landed in the emitted consts.
    assert_eq!(WORKERS_MIN, 1);
    assert_eq!(WORKERS_MAX, 2);
    assert_eq!(WORKERS_MEMBERS, 2);

    let sup = Supervisor::new(&GRAPH);

    // Floor comes up with ONLY the floor member's element provided.
    CONN[0].provide(Conn { id: 10, uses: 0 });
    sup.start(spawner).await.expect("start with floor element");
    settle(|| SPAWNS[0].load(Ordering::SeqCst) == 1).await;
    assert_eq!(SEEN_ID[0].load(Ordering::SeqCst), 10);

    // Growth wants member 1 (floor is busy) but its element is empty: the
    // spawn fail-closes (Busy, once the mock clock passes the gate timeout)
    // and is simply re-driven, so the member stays down until ITS element is
    // provided.
    let mut budget = 0;
    drive_until(&sup, spawner, || {
        budget += 1;
        budget >= 300
    })
    .await;
    assert_eq!(
        SPAWNS[1].load(Ordering::SeqCst),
        0,
        "burst member waits for its OWN element"
    );

    CONN[1].provide(Conn { id: 20, uses: 0 });
    embassy_supervisor::request_scale();
    drive_until(&sup, spawner, || SPAWNS[1].load(Ordering::SeqCst) == 1).await;
    assert_eq!(SEEN_ID[1].load(Ordering::SeqCst), 20, "member 1 got id 20");
    assert_eq!(SEEN_ID[0].load(Ordering::SeqCst), 10, "member 0 kept id 10");

    // Restore-to-same-index: stop member 1 (run_pools is NOT running here, so
    // nothing races the handshake); the shell restores its element (use count
    // bumped by the first run). Peek it, put it back, then drive the pool
    // again — floor still busy, so the regrow re-takes the SAME instance from
    // the same index.
    sup.stop_node(&WORKERS[1]).await.expect("member 1 acks");
    let back = CONN[1].take().expect("restored to element 1");
    assert_eq!((back.id, back.uses), (20, 1), "same instance, one use");
    CONN[1].restore(back);
    embassy_supervisor::request_scale();
    drive_until(&sup, spawner, || SPAWNS[1].load(Ordering::SeqCst) == 2).await;
    assert_eq!(SEEN_ID[1].load(Ordering::SeqCst), 20, "same Conn re-taken");

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn per_member_pool_resources() {
    let clock = embassy_time::MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        // Keep the mock clock moving: the empty-element growth attempt parks on
        // its 100 ms gate timeout inside the pool driver, and only the timeout
        // firing (SpawnError::Busy) lets the driver breathe and re-drive.
        clock.advance(embassy_time::Duration::from_millis(50));
        assert!(
            StdInstant::now() < deadline,
            "did not complete (spawns=[{},{}])",
            SPAWNS[0].load(Ordering::SeqCst),
            SPAWNS[1].load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
