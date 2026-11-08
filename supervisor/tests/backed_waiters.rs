//! Five readers parked in `open` on one producer: the serving event holds four
//! wakers and the fifth reader waits out its retry timer, so with the clock
//! frozen every reader stays parked instead of waking in a storm.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Backed, Supervisor, TaskNode, dataflow, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, MockDriver};

pub static EST: Backed<AtomicU32> = Backed::new(AtomicU32::new(0));
static GO: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static POLLS: AtomicU32 = AtomicU32::new(0);
static OPENED: AtomicU32 = AtomicU32::new(0);

supervisor_graph! {
    node PRODUCER = Terminate, deps: [], task: producer, discover;
    node R0 = Terminate, deps: [], task: r0, dataflow: [crate::reader];
    node R1 = Terminate, deps: [], task: r1, dataflow: [crate::reader];
    node R2 = Terminate, deps: [], task: r2, dataflow: [crate::reader];
    node R3 = Terminate, deps: [], task: r3, dataflow: [crate::reader];
    node R4 = Terminate, deps: [], task: r4, dataflow: [crate::reader];
}

#[dataflow]
async fn producer(node: &'static TaskNode) {
    node.writer(&crate::EST).store(1, Ordering::SeqCst);
    // Running but not ready until the test says so: every reader stays gated.
    GO.wait().await;
    node.set_ready();
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// Counts every poll of its `open`.
#[dataflow]
async fn reader(node: &'static TaskNode) {
    let mut open = core::pin::pin!(node.open(&crate::EST));
    let _guard = core::future::poll_fn(|cx| {
        POLLS.fetch_add(1, Ordering::SeqCst);
        open.as_mut().poll(cx)
    })
    .await;
    OPENED.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn r0(node: &'static TaskNode) {
    reader(node).await
}
async fn r1(node: &'static TaskNode) {
    reader(node).await
}
async fn r2(node: &'static TaskNode) {
    reader(node).await
}
async fn r3(node: &'static TaskNode) {
    reader(node).await
}
async fn r4(node: &'static TaskNode) {
    reader(node).await
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
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    let fault = sup.run(&spawner).await;
    panic!("driver returned: {fault}");
}

#[test]
fn five_gated_readers_park_without_a_wake_storm() {
    let clock = MockDriver::get();
    let mut thread = Some(std::thread::spawn(|| {
        let executor = Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(runner(spawner).unwrap());
            spawner.spawn(pass_probe().unwrap());
        });
    }));
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    let mut wait_for = |what: &str, cond: &dyn Fn() -> bool| {
        while !cond() {
            if let Some(thread) = thread.take_if(|t| t.is_finished()) {
                thread.join().expect("executor assertions failed");
                panic!("executor exited before completion");
            }
            assert!(
                StdInstant::now() < deadline,
                "{what} (polls={}, opened={})",
                POLLS.load(Ordering::SeqCst),
                OPENED.load(Ordering::SeqCst)
            );
            std::thread::sleep(StdDuration::from_millis(2));
        }
    };
    wait_for("every reader polled", &|| POLLS.load(Ordering::SeqCst) >= 5);
    let parked = POLLS.load(Ordering::SeqCst);
    let seen = PASSES.load(Ordering::SeqCst);
    wait_for("100 executor passes", &|| {
        PASSES.load(Ordering::SeqCst) >= seen + 100
    });
    assert_eq!(
        POLLS.load(Ordering::SeqCst),
        parked,
        "parked readers are not polled again: no wake storm"
    );
    PROBE_STOP.store(true, Ordering::SeqCst);
    GO.signal(());
    wait_for("the registered readers pass", &|| {
        OPENED.load(Ordering::SeqCst) >= 4
    });
    clock.advance(Duration::from_millis(250));
    wait_for("the fifth reader passes on its retry", &|| {
        OPENED.load(Ordering::SeqCst) == 5
    });
}
