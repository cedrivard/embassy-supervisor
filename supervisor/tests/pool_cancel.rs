//! `cancel` on a `pool`: the flag rides the ONE shell every member shares, so a
//! plain supervisor-unaware worker can be *scaled* — an elastic shrink drops the
//! surplus member's future in place and the shell answers the handshake for it.
//!
//! That is the whole point here: `worker` below is diverging, takes no
//! `&TaskNode`, and contains no handshake, so without `cancel` the shrink would
//! come back as `ShutdownTimeout` and `run_pools` would abort. What is proven:
//! the shrink completes, the member's per-member resource is restored to its own
//! slot index (so the regrow re-takes the same instance), and the dropped future
//! really stops advancing. Harness as pool_member_resources.rs.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{DeferredShrink, Supervisor, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

/// A per-member connection resource. `id` doubles as the member index (the
/// worker has no node, so `member_index` isn't available to it — carrying the
/// index in the resource is the shape a `cancel` pool uses), and `uses`
/// accumulates inside the instance so a regrow can prove it got the same one.
struct Conn {
    id: u32,
    uses: u32,
}

supervisor_graph! {
    pool WORKERS = [Terminate, OnDemand], deps: [],
        task: worker,
        resources: [CONN: Conn],
        policy: DeferredShrink::new(embassy_time::Duration::from_millis(50)),
        min: 1, max: 2, cancel;
}

static SPAWNS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static ROUNDS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static SEEN_USES: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static WORK: [Signal<CriticalSectionRawMutex, ()>; 2] = [Signal::new(), Signal::new()];
static DONE: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU32 = AtomicU32::new(0);

/// The worker a firmware already has: a plain `async fn` that serves forever.
/// No node, no ack, no supervisor in its signature — its member's own `Conn`
/// arrives first because `cancel` suppresses the node lead.
async fn worker(conn: &mut Conn) -> ! {
    let i = conn.id as usize;
    conn.uses += 1;
    SEEN_USES[i].store(conn.uses, Ordering::SeqCst);
    SPAWNS[i].fetch_add(1, Ordering::SeqCst);
    loop {
        ROUNDS[i].fetch_add(1, Ordering::SeqCst);
        WORK[i].wait().await;
    }
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

/// Drive the pool until `until()` holds. A shrink the member never acks surfaces
/// here as `run_pools` returning — which is exactly the failure `cancel` is
/// meant to remove, so the panic message names it.
///
/// The budget is spent in MOCK time, not in polls: a deferred shrink parks
/// `run_pools` on `Timer::at(cooldown)`, and the mock clock only moves when the
/// test thread advances it. A spin budget would therefore race that thread (and
/// lose on a loaded machine — 10k yields cost microseconds, one clock advance
/// costs milliseconds), so each pass waits for the clock instead of the CPU.
async fn drive_until<const N: usize>(
    sup: &Supervisor<N>,
    spawner: Spawner,
    mut until: impl FnMut() -> bool,
) {
    use embassy_futures::select::{Either, select};
    let watch = async {
        for _ in 0..400 {
            if until() {
                return;
            }
            embassy_time::Timer::after(embassy_time::Duration::from_millis(5)).await;
        }
    };
    match select(sup.run_pools(spawner), watch).await {
        Either::First(err) => panic!("member never acked: {:?}", err.node.name),
        Either::Second(()) => {}
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    CONN[0].provide(Conn { id: 0, uses: 0 });
    CONN[1].provide(Conn { id: 1, uses: 0 });
    sup.start(spawner).await.expect("floor member starts");
    settle(|| SPAWNS[0].load(Ordering::SeqCst) == 1).await;
    PHASE.store(1, Ordering::SeqCst);

    // Grow. A `cancel` worker can't report its own load (it holds no node), so
    // the busy signal comes from outside — the member statics are app-visible.
    WORKERS[0].mark_busy();
    drive_until(&sup, spawner, || SPAWNS[1].load(Ordering::SeqCst) == 1).await;
    assert_eq!(
        SEEN_USES[1].load(Ordering::SeqCst),
        1,
        "member 1 took CONN[1]"
    );
    PHASE.store(2, Ordering::SeqCst);

    // ── the shrink: two idle members past the floor, held for the cooldown.
    //    Reaching the assert at all means the surplus member acked, and the
    //    worker has no code that could have done that ─────────────────────────
    WORKERS[0].mark_idle();
    drive_until(&sup, spawner, || !WORKERS[1].is_running()).await;
    assert!(!WORKERS[1].is_running(), "surplus member shrunk");
    assert!(WORKERS[1].has_exited(), "and recorded as exited");
    assert!(WORKERS[0].is_running(), "floor member untouched");
    PHASE.store(3, Ordering::SeqCst);

    // The dropped future is gone, not merely unobserved.
    let rounds_before = ROUNDS[1].load(Ordering::SeqCst);
    WORK[1].signal(());
    settle(|| false).await;
    assert_eq!(
        ROUNDS[1].load(Ordering::SeqCst),
        rounds_before,
        "the aborted member is dropped"
    );

    // The shell's tail ran on the shrink path: `CONN[1]` is back in ITS element
    // (index 1) with the use count the aborted run left on it, so the regrow
    // re-takes that instance instead of finding an empty slot.
    WORKERS[0].mark_busy();
    drive_until(&sup, spawner, || SPAWNS[1].load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        SEEN_USES[1].load(Ordering::SeqCst),
        2,
        "same Conn restored to index 1 and re-taken (a fresh one would read 1)"
    );
    assert_eq!(
        SEEN_USES[0].load(Ordering::SeqCst),
        1,
        "the floor member kept its own instance throughout"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn cancel_pool_members_shrink_and_restore() {
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
        // The shrink is deferred on a cooldown against this clock.
        clock.advance(embassy_time::Duration::from_millis(50));
        assert!(
            StdInstant::now() < deadline,
            "did not complete (phase={}, spawns=[{},{}])",
            PHASE.load(Ordering::SeqCst),
            SPAWNS[0].load(Ordering::SeqCst),
            SPAWNS[1].load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
