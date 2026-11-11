mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

static FLAG: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU32 = AtomicU32::new(0);
static SOLO_CALLS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

fn flag() -> bool {
    CALLS.fetch_add(1, Ordering::SeqCst);
    FLAG.load(Ordering::SeqCst)
}

fn solo_flag() -> bool {
    SOLO_CALLS.fetch_add(1, Ordering::SeqCst);
    false
}

fn never() -> bool {
    false
}

supervisor_graph! {
    node GATED = Terminate, deps: [], task: worker, enabled_if: flag;
    node ALWAYS = Terminate, deps: [], task: worker;
    node SOLO = Terminate, deps: [], task: worker, enabled_if: solo_flag;
    // `all()` keeps a gated clause, `any()` strips it.
    node KEPT = Terminate, deps: [], task: worker, #[cfg(all())] enabled_if: never;
    node STRIPPED = Terminate, deps: [], task: worker, #[cfg(any())] enabled_if: never;
}

async fn worker(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn settle() {
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    // Self-managed for the whole test: start() must never ask its predicate.
    SOLO.set_detached(true);

    sup.start(&spawner).await.expect("start");
    settle().await;
    assert!(ALWAYS.is_running());
    assert!(!GATED.is_running(), "a false predicate keeps the node down");
    assert!(GATED.is_disabled(), "and latches it disabled, visibly");
    assert_eq!(CALLS.load(Ordering::SeqCst), 1, "evaluated once per start");
    assert_eq!(
        SOLO_CALLS.load(Ordering::SeqCst),
        0,
        "a detached node's predicate is never called"
    );
    assert!(
        !SOLO.is_running() && !SOLO.is_disabled(),
        "nor is it started or latched"
    );
    assert!(
        KEPT.is_disabled() && !KEPT.is_running(),
        "an all()-gated clause is kept"
    );
    assert!(
        STRIPPED.is_running() && !STRIPPED.is_disabled(),
        "an any()-gated clause is stripped"
    );

    sup.activate(&GATED, &spawner)
        .await
        .expect("activate overrides the latch");
    settle().await;
    assert!(GATED.is_running() && !GATED.is_disabled());

    sup.start(&spawner).await.expect("re-entry");
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "a running node is not re-evaluated, so re-entry cannot latch it"
    );
    assert!(GATED.is_running() && !GATED.is_disabled());

    sup.teardown().await.expect("teardown");
    FLAG.store(true, Ordering::SeqCst);
    sup.start(&spawner).await.expect("start");
    settle().await;
    assert!(GATED.is_running(), "a true predicate brings it up");

    sup.teardown().await.expect("teardown");
    FLAG.store(false, Ordering::SeqCst);
    let calls = CALLS.load(Ordering::SeqCst);
    sup.respawn_terminate(&spawner).await.expect("respawn");
    settle().await;
    assert!(
        GATED.is_running() && !GATED.is_disabled(),
        "respawn_terminate brings a Terminate node back without asking enabled_if"
    );
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        calls,
        "the predicate was not called"
    );
    FLAG.store(true, Ordering::SeqCst);

    sup.teardown().await.expect("teardown");
    GATED.set_disabled(true);
    sup.start(&spawner).await.expect("start");
    settle().await;
    assert!(
        !GATED.is_running() && GATED.is_disabled(),
        "a true predicate never clears a manual latch"
    );
    assert!(ALWAYS.is_running());

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn enabled_if_latches_disabled_at_start() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || "did not complete".to_owned(),
    );
}
