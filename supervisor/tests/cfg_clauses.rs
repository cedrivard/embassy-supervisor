//! `#[cfg(...)]`-gated value-level clauses, exercised through a real
//! bring-up. `all()` is the always-true predicate (the clause is active,
//! through the gated emission path); `any()` is always false (rustc strips
//! the clause, and the node behaves as if it were never written).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};

static SPAWNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode) {
    SPAWNS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.mark_exited();
}

supervisor_graph! {
    node PARKED = Terminate, deps: [], task: worker,
        #[cfg(all())] disabled,
        #[cfg(all())] slot_timeout: 250;
    node LIVE = Terminate, deps: [], task: worker,
        #[cfg(any())] disabled,
        #[cfg(any())] slot_timeout: 250,
        #[cfg(any())] ack_timeout: 900;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    // The gates resolve before the graph exists: an active `disabled` is a
    // boot-time latch, a stripped one leaves the default (enabled), and the
    // gated timeouts land in (or stay out of) the node's config.
    assert!(PARKED.is_disabled(), "an `all()`-gated `disabled` latches");
    assert!(
        !LIVE.is_disabled(),
        "an `any()`-gated `disabled` is stripped"
    );
    assert_eq!(
        PARKED.slot_timeout(),
        embassy_time::Duration::from_millis(250)
    );
    assert_ne!(
        LIVE.slot_timeout(),
        embassy_time::Duration::from_millis(250),
        "the stripped clause leaves the default gate wait"
    );

    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    assert!(
        !PARKED.is_running(),
        "disabled at boot, skipped by the wave"
    );
    assert!(LIVE.is_running());
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
    assert_eq!(SPAWNS.load(Ordering::SeqCst), 1);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn gated_clauses_resolve_per_build() {
    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
