//! Two supervisors' waves parked at once on the shared gate and stop events:
//! one event wakes both, with the clock frozen so no deadline can stand in.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

static UP: AtomicU32 = AtomicU32::new(0);
static STARTED: AtomicU32 = AtomicU32::new(0);
static STOPPED: AtomicU32 = AtomicU32::new(0);
/// Set by the second wave to stop.
static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode, _slot: &mut u32) {
    UP.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.mark_exited();
}

supervisor_graph! {
    node A = Terminate, deps: [], task: worker, resources: [A_SLOT: u32], slot_timeout: 5000;
}
supervisor_graph! {
    name: OTHER;
    node B = Terminate, deps: [], task: worker, resources: [B_SLOT: u32], slot_timeout: 5000;
}

async fn until(cond: impl Fn() -> bool) {
    while !cond() {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn first(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner)
        .await
        .expect("A's wave sees its slot fill");
    STARTED.fetch_add(1, Ordering::SeqCst);
    until(|| STARTED.load(Ordering::SeqCst) == 2).await;
    sup.teardown().await.expect("A's wave sees its ack");
    if STOPPED.fetch_add(1, Ordering::SeqCst) == 1 {
        DONE.store(true, Ordering::SeqCst);
    }
}

#[embassy_executor::task]
async fn second(spawner: Spawner) {
    let sup = Supervisor::new(&OTHER);
    sup.start(&spawner)
        .await
        .expect("B's wave sees its slot fill");
    STARTED.fetch_add(1, Ordering::SeqCst);
    until(|| STARTED.load(Ordering::SeqCst) == 2).await;
    sup.teardown().await.expect("B's wave sees its ack");
    if STOPPED.fetch_add(1, Ordering::SeqCst) == 1 {
        DONE.store(true, Ordering::SeqCst);
    }
}

/// Fills both slots in one poll, once both waves are parked.
#[embassy_executor::task]
async fn provider() {
    for _ in 0..32 {
        embassy_futures::yield_now().await;
    }
    A_SLOT.provide(1);
    B_SLOT.provide(2);
}

#[test]
fn one_event_wakes_every_supervisors_wave() {
    // Frozen clock: no deadline fires.
    support::run(
        |spawner| {
            spawner.spawn(first(spawner).unwrap());
            spawner.spawn(second(spawner).unwrap());
            spawner.spawn(provider().unwrap());
        },
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "a wave missed the shared event (up={}, started={}, stopped={})",
                UP.load(Ordering::SeqCst),
                STARTED.load(Ordering::SeqCst),
                STOPPED.load(Ordering::SeqCst)
            )
        },
    );
    assert_eq!(UP.load(Ordering::SeqCst), 2);
}
