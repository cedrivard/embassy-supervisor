//! A `Hog` cleared inside its grace and injected again later gets a fresh
//! 250 ms grace: the cleared fault's timer is dropped, never reused, so the
//! spin cannot start the moment the second hog lands.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Fault, Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node HOGGER = Terminate, deps: [], task: hogger;
}

/// 0 setup, 1 asks the test thread to advance past the first grace, 2 that
/// advance is done, 3 the test thread steps the clock until the hog ends.
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn hogger(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn until(cond: impl Fn() -> bool) {
    while !cond() {
        embassy_futures::yield_now().await;
    }
}

async fn yields(n: usize) {
    for _ in 0..n {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("start");
    yields(8).await;
    HOGGER
        .inject(Fault::Hog(Duration::from_millis(1)))
        .expect("a task: shell takes a hog");
    yields(8).await;
    HOGGER.clear_fault();
    yields(8).await;
    PHASE.store(1, Ordering::SeqCst);
    until(|| PHASE.load(Ordering::SeqCst) == 2).await;
    HOGGER
        .inject(Fault::Hog(Duration::from_millis(1)))
        .expect("a task: shell takes a hog");
    yields(8).await;
    assert_eq!(
        HOGGER.fault(),
        Fault::Hog(Duration::from_millis(1)),
        "still in its fresh grace"
    );
    PHASE.store(3, Ordering::SeqCst);
    until(|| HOGGER.fault() == Fault::None).await;
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_reinjected_hog_gets_a_fresh_grace() {
    let clock = MockDriver::get();
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
        assert!(
            StdInstant::now() < deadline,
            "the re-injected hog skipped its grace (phase={})",
            PHASE.load(Ordering::SeqCst)
        );
        match PHASE.load(Ordering::SeqCst) {
            1 => {
                clock.advance(Duration::from_millis(300));
                PHASE.store(2, Ordering::SeqCst);
            }
            3 => clock.advance(Duration::from_millis(10)),
            _ => {}
        }
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
