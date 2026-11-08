//! The handoff respects the live instance: `start_node` leaves a running or
//! detached node alone, and an instance that acks before its spawn returns
//! (a spawn closure standing in for a preempting first poll) is not marked
//! running, by the wave or by a single-node start.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph};
use embassy_time::MockDriver;

supervisor_graph! {
    node HELD = Terminate, deps: [];
    node LOOSE = Terminate, deps: [], disabled;
    node QUICK = Terminate, deps: [], spawn: |_s| {
        crate::QUICK.mark_exited();
        Ok(())
    };
    node QUICK_LATER = Terminate, deps: [], disabled, spawn: |_s| {
        crate::QUICK_LATER.mark_exited();
        Ok(())
    };
}

static DONE: AtomicBool = AtomicBool::new(false);

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start");
    assert!(
        QUICK.has_exited() && !QUICK.is_running(),
        "the wave does not mark running an instance that already exited"
    );
    sup.start_node(&QUICK_LATER, &spawner)
        .await
        .expect("start_node");
    assert!(
        QUICK_LATER.has_exited() && !QUICK_LATER.is_running(),
        "nor does a single-node start"
    );

    assert!(HELD.is_running());
    HELD.mark_busy();
    sup.start_node(&HELD, &spawner)
        .await
        .expect("a running node is a no-op");
    assert!(
        HELD.is_running() && HELD.is_busy(),
        "start_node left the live instance alone (no reset)"
    );

    LOOSE.set_detached(true);
    sup.start_node(&LOOSE, &spawner)
        .await
        .expect("a detached node is a no-op");
    assert!(
        !LOOSE.is_running(),
        "a detached node is the application's to start"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn the_handoff_respects_the_instance() {
    let _clock = MockDriver::get();
    let thread = std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        if thread.is_finished() {
            thread.join().expect("executor assertions failed");
            panic!("executor exited before completion");
        }
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
