//! The inverted layout: the supervisor runs on its own executor (thread B, the
//! "interrupt tier") and places the graph's default tier on another executor
//! (thread A, "thread mode") through `default executor THREAD;` filled with
//! `spawner.make_send()`. A `local` provider/consumer pair inherits the default
//! too, so the slot's whole lifecycle happens off the supervisor's thread.

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
static LOCAL_TIER: AtomicU8 = AtomicU8::new(0);
static LOCAL_SEEN: AtomicU32 = AtomicU32::new(0);
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
    BLOB.provide(Rc::new(Cell::new(7)));
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn local_consumer(node: &'static TaskNode, blob: Rc<Cell<u32>>) {
    LOCAL_TIER.store(tier(), Ordering::Release);
    LOCAL_SEEN.store(blob.get(), Ordering::Release);
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

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner)
        .await
        .expect("start rendezvouses with thread A's late publish");
    while ON_THREAD_TIER.load(Ordering::Acquire) == 0
        || ON_SUP_TIER.load(Ordering::Acquire) == 0
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

#[test]
fn supervisor_on_its_own_tier_places_the_default_elsewhere() {
    // Thread A: the graph's default tier, published 50 ms late so `start`
    // exercises the slot rendezvous.
    std::thread::spawn(|| {
        TIER.with(|t| t.set(1));
        std::thread::sleep(StdDuration::from_millis(50));
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| THREAD.set(spawner.make_send()));
    });
    // Thread B: the supervisor's own executor, also published as a slot so a
    // node can ask for it explicitly.
    std::thread::spawn(|| {
        TIER.with(|t| t.set(2));
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            SUP.set(spawner.make_send());
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::Acquire) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (tiers: thread={}, sup={}, local={})",
            ON_THREAD_TIER.load(Ordering::Acquire),
            ON_SUP_TIER.load(Ordering::Acquire),
            LOCAL_TIER.load(Ordering::Acquire),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
