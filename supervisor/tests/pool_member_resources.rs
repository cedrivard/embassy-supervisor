use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph};

const FLOOR: usize = 1;

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
    assert!(f(), "did not settle");
}

/// Drive the pool until `until()` holds (panicking if a generous yield budget
/// elapses first). Dropping `run_pools` mid-flight leaves a half-applied action
/// to the next pass (its event and ack check are lost; this test reads
/// neither), and keeping ALL pool driving inside the one driver task is the
/// production shape: a concurrent pool task would race the manual stop/start
/// below on the member's handshake flags.
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
        Either::First(err) => panic!("pool shrink timed out: {:?}", err.node.name()),
        Either::Second(()) => {}
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    assert_eq!(WORKERS_MIN, 1);
    assert_eq!(WORKERS_MAX, 2);
    assert_eq!(WORKERS_MEMBERS, 2);

    let sup = Supervisor::new(&GRAPH);

    CONN[0].provide(Conn { id: 10, uses: 0 });
    sup.start(&spawner).await.expect("start with floor element");
    settle(|| SPAWNS[0].load(Ordering::SeqCst) == 1).await;
    assert_eq!(SEEN_ID[0].load(Ordering::SeqCst), 10);

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

    sup.stop_node(&WORKERS[1]).await.expect("member 1 acks");
    let back = CONN[1].take().expect("restored to element 1");
    assert_eq!((back.id, back.uses), (20, 1), "same instance, one use");
    CONN[1].restore(back);
    embassy_supervisor::request_scale();
    drive_until(&sup, spawner, || SPAWNS[1].load(Ordering::SeqCst) == 2).await;

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
        // Keep the mock clock moving: a growth attempt on the still-empty
        // element 1 waits in the member's resource gate for its `slot_timeout`
        // (100 ms default) and faults `ResourceMissing`, which the pool driver
        // discards before waiting for the next scale request; with time
        // frozen it would stay parked until `drive_until` drops it.
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
