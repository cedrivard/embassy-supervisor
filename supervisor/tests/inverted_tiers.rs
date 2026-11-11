//! The inverted layout: the supervisor runs on its own executor (thread B, the
//! "interrupt tier") and places the graph's default tier on another executor
//! (thread A, "thread mode") through `default executor THREAD;` filled with
//! `spawner.make_send()`. A `local` provider/consumer pair inherits the default
//! too, so the slot's whole lifecycle happens off the supervisor's thread.

mod support;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

thread_local! {
    /// Which executor thread is polling: 1 = A (the graph default), 2 = B (the
    /// supervisor's own).
    static TIER: Cell<u8> = const { Cell::new(0) };
}

static ON_THREAD_TIER: AtomicU8 = AtomicU8::new(0);
static ON_SUP_TIER: AtomicU8 = AtomicU8::new(0);
static LOCAL_P_TIER: AtomicU8 = AtomicU8::new(0);
static LOCAL_TIER: AtomicU8 = AtomicU8::new(0);
static LOCAL_SEEN: AtomicU32 = AtomicU32::new(0);
static START_ENTERED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

fn tier() -> u8 {
    TIER.with(Cell::get)
}

async fn on_thread_worker(node: &'static TaskNode) {
    ON_THREAD_TIER.store(tier(), Ordering::Release);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn on_sup_worker(node: &'static TaskNode) {
    ON_SUP_TIER.store(tier(), Ordering::Release);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn local_provider(node: &'static TaskNode) {
    LOCAL_P_TIER.store(tier(), Ordering::Release);
    // SAFETY: called on the slot's executor: the graph places this provider
    // and the slot's consumer on the default tier.
    unsafe { BLOB.provide(Rc::new(Cell::new(7))) };
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn local_consumer(node: &'static TaskNode, blob: Rc<Cell<u32>>) {
    // The value before the tier: the driver waits on the tier, then reads it.
    LOCAL_SEEN.store(blob.get(), Ordering::Release);
    LOCAL_TIER.store(tier(), Ordering::Release);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    default executor THREAD;
    executor SUP;
    node ON_THREAD = Terminate, deps: [], task: on_thread_worker;
    node ON_SUP = Terminate, deps: [], executor: SUP, task: on_sup_worker;
    node LOCAL_P = Terminate, deps: [], task: local_provider, provides: [BLOB];
    node LOCAL_C = Terminate, deps: [LOCAL_P], task: local_consumer,
        resources: [BLOB: local consume Rc<Cell<u32>>];
}

/// Executor passes: the probe is polled once per pass, so a move of N means
/// every task woken before it has been polled N times over.
static PASSES: AtomicU32 = AtomicU32::new(0);
static PROBE_STOP: AtomicBool = AtomicBool::new(false);

/// Ends after the test's last barrier, so the executor idles instead of
/// spinning for the life of the binary.
#[embassy_executor::task]
async fn pass_probe() {
    while !PROBE_STOP.load(Ordering::SeqCst) {
        PASSES.fetch_add(1, Ordering::SeqCst);
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    START_ENTERED.store(true, Ordering::Release);
    sup.start(&spawner)
        .await
        .expect("start rendezvouses with thread A's late publish");
    while ON_THREAD_TIER.load(Ordering::Acquire) == 0
        || ON_SUP_TIER.load(Ordering::Acquire) == 0
        || LOCAL_P_TIER.load(Ordering::Acquire) == 0
        || LOCAL_TIER.load(Ordering::Acquire) == 0
    {
        embassy_futures::yield_now().await;
    }
    assert_eq!(
        ON_THREAD_TIER.load(Ordering::Acquire),
        1,
        "inherited the default tier"
    );
    assert_eq!(
        ON_SUP_TIER.load(Ordering::Acquire),
        2,
        "explicit slot wins over the default"
    );
    assert_eq!(
        LOCAL_P_TIER.load(Ordering::Acquire),
        1,
        "the local provider ran on tier A too"
    );
    assert_eq!(
        LOCAL_TIER.load(Ordering::Acquire),
        1,
        "the local consumer inherited too"
    );
    assert_eq!(
        LOCAL_SEEN.load(Ordering::Acquire),
        7,
        "the local value crossed provider to consumer on tier A"
    );
    assert!(ON_THREAD.is_running() && ON_SUP.is_running());

    sup.stop_node(&ON_THREAD).await.expect("cross-thread stop");
    sup.stop_node(&LOCAL_C)
        .await
        .expect("cross-thread stop of the local consumer");
    sup.stop_node(&ON_SUP).await.expect("same-thread stop");
    assert!(!ON_THREAD.is_running() && !ON_SUP.is_running() && !LOCAL_C.is_running());
    DONE.store(true, Ordering::Release);
}

/// Holds the late slot publish until the driver is inside `start` and its
/// executor has run 50 more passes: `start` is then parked on the empty
/// slot, so the rendezvous branch is the one taken.
fn wait_until_start_parked() {
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !START_ENTERED.load(Ordering::Acquire) {
        assert!(
            StdInstant::now() < deadline,
            "the driver never entered start()"
        );
        std::thread::sleep(StdDuration::from_millis(1));
    }
    let seen = PASSES.load(Ordering::Acquire);
    while PASSES.load(Ordering::Acquire) < seen + 50 {
        assert!(
            StdInstant::now() < deadline,
            "the driver's executor stalled"
        );
        std::thread::sleep(StdDuration::from_millis(1));
    }
    PROBE_STOP.store(true, Ordering::SeqCst);
}

#[test]
fn supervisor_on_its_own_tier_places_the_default_elsewhere() {
    // Thread A: the graph's default tier, published only once `start` is parked
    // on its empty slot, so `start` exercises the rendezvous.
    std::thread::spawn(|| {
        TIER.with(|t| t.set(1));
        wait_until_start_parked();
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| THREAD.set(spawner.make_send()));
    });
    // Thread B: the supervisor's own executor, also published as a slot so a
    // node can ask for it explicitly. `init` runs on the executor's own thread.
    support::run(
        |spawner| {
            TIER.with(|t| t.set(2));
            SUP.set(spawner.make_send());
            spawner.spawn(driver(spawner).unwrap());
            spawner.spawn(pass_probe().unwrap());
        },
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (tiers: thread={}, sup={}, local={})",
                ON_THREAD_TIER.load(Ordering::Acquire),
                ON_SUP_TIER.load(Ordering::Acquire),
                LOCAL_TIER.load(Ordering::Acquire),
            )
        },
    );
}
