//! The handoff publishes after the activation clears: an instance that runs
//! before `spawn` returns (a higher tier, another core) keeps its first words
//! and, if it already exited, stays down. A spawn closure stands in for that
//! first poll, deterministically.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph};

static QUICK_SPAWNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

supervisor_graph! {
    node QUICK = Terminate, deps: [], spawn: |_s| {
        crate::QUICK_SPAWNS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        crate::QUICK.mark_exited();
        Ok(())
    };
    node CHATTY = Terminate, deps: [], spawn: |_s| {
        crate::CHATTY.report_status("booting");
        Ok(())
    };
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start");
    assert!(QUICK.has_exited(), "the instance exited during the handoff");
    assert!(!QUICK.is_running(), "and is not marked running after it");
    assert_eq!(
        CHATTY.status(),
        Some("booting"),
        "first words survive the publish"
    );
    assert!(CHATTY.is_running());
    sup.respawn_terminate(&spawner).await.expect("respawn");
    assert_eq!(
        QUICK_SPAWNS.load(Ordering::SeqCst),
        2,
        "a down node is respawnable"
    );
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn the_supervisor_publishes_after_the_activation_clears() {
    let _clock = embassy_time::MockDriver::get();
    let thread = std::thread::spawn(|| {
        let executor = Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| spawner.spawn(driver(spawner).unwrap()));
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        if thread.is_finished() {
            thread.join().expect("executor assertions failed");
            panic!("executor exited before completion");
        }
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
