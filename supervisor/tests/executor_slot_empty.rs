//! A node placed on an executor whose slot is never filled waits out its gate
//! budget, then `start` reports `ExecutorSlotEmpty`: with the clock frozen it
//! stays pending, and it faults once the clock passes `slot_timeout`.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration as StdDuration;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, Instant, MockDriver};

static RUNS: AtomicU32 = AtomicU32::new(0);
/// Set once the frozen-clock phase is over: the test thread then moves the
/// clock past X's gate budget.
static ADVANCE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode) {
    RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

supervisor_graph! {
    executor NEVER;
    node X = Terminate, deps: [], executor: NEVER, task: worker, slot_timeout: 100;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    let t0 = Instant::now();
    let mut start = core::pin::pin!(sup.start(&spawner));
    let script = async {
        for _ in 0..1000 {
            embassy_futures::yield_now().await;
        }
    };
    if let Either::First(r) = select(start.as_mut(), script).await {
        panic!("start returned with the clock frozen: {r:?}");
    }

    ADVANCE.store(true, Ordering::SeqCst);
    let fault = start.await.expect_err("NEVER is never filled");
    assert!(
        matches!(fault.kind, FaultKind::ExecutorSlotEmpty),
        "the empty slot is named: {fault}"
    );
    assert_eq!(fault.node.name(), "x");
    assert!(
        format!("{fault}").contains("executor slot was never filled"),
        "{fault}"
    );
    assert!(
        Instant::now() - t0 >= Duration::from_millis(100),
        "the fault waited out the 100 ms slot_timeout"
    );
    assert!(!X.is_running());
    assert_eq!(RUNS.load(Ordering::SeqCst), 0, "the worker never ran");
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn an_unfilled_executor_slot_faults_after_its_gate_budget() {
    let clock = MockDriver::get();
    std::thread::spawn(move || {
        while !ADVANCE.load(Ordering::SeqCst) {
            std::thread::sleep(StdDuration::from_millis(5));
        }
        clock.advance(Duration::from_millis(200));
    });
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "did not complete (runs={}, running={})",
                RUNS.load(Ordering::SeqCst),
                X.is_running(),
            )
        },
    );
}
