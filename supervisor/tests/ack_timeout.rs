use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, MockDriver};

static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

static GO: Signal<CriticalSectionRawMutex, ()> = Signal::new();

async fn wedged_worker(_node: &'static TaskNode) {
    core::future::pending::<()>().await;
}

/// Acks the stop only when the driver says so — after the default window.
async fn slow_worker(node: &'static TaskNode) {
    let _ = node.run_cancellable(core::future::pending::<()>()).await;
    GO.wait().await;
    node.ack_dropped();
}

supervisor_graph! {
    node WEDGE = Terminate, deps: [], task: wedged_worker, ack_timeout: 100;
    node SLOW = Terminate, deps: [], task: slow_worker, ack_timeout: 5000;
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    settle(|| WEDGE.is_running() && SLOW.is_running()).await;
    PHASE.store(1, Ordering::SeqCst);

    let t0 = Instant::now();
    let err = sup.stop_node(&WEDGE).await.expect_err("never acks");
    assert_eq!(err.node.name(), "wedge");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout));
    assert!(
        Instant::now() - t0 < Duration::from_millis(2_000),
        "the 100 ms window applied, not the 2 s default"
    );
    PHASE.store(2, Ordering::SeqCst);

    let t0 = Instant::now();
    let (res, _) = embassy_futures::join::join(sup.stop_node(&SLOW), async {
        while Instant::now() - t0 < Duration::from_millis(2_500) {
            embassy_futures::yield_now().await;
        }
        GO.signal(());
    })
    .await;
    res.expect("acked inside the raised 5 s window");
    assert!(!SLOW.is_running());
    assert!(
        Instant::now() - t0 >= Duration::from_millis(2_500),
        "the ack really was held past the default window"
    );
    PHASE.store(3, Ordering::SeqCst);

    let t0 = Instant::now();
    let err = sup
        .teardown_continue()
        .await
        .expect_err("the wedge is still the reported fault");
    assert_eq!(err.node.name(), "wedge");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout));
    assert!(
        Instant::now() - t0 < Duration::from_millis(2_000),
        "the wave budgeted the wedge at 100 ms, not the 2 s default"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn ack_timeout_bounds_both_stop_paths() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        // Steps well under WEDGE's 100 ms window, so a fault always lands
        if PHASE.load(Ordering::SeqCst) >= 1 {
            clock.advance(embassy_time::Duration::from_millis(25));
        }
        assert!(
            StdInstant::now() < deadline,
            "did not complete (phase={}, wedge_running={}, slow_running={})",
            PHASE.load(Ordering::SeqCst),
            WEDGE.is_running(),
            SLOW.is_running(),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
