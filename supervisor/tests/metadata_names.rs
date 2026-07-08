//! Behavioral test for the `metadata-names` feature WITHOUT `trace`.
//!
//! This is the regression guard for the name-stamping/trace decoupling: with only
//! `metadata-names` (not `trace`) the macro must emit the name-only spawn path
//! (`stamp_name`, no `adopt`, no id capture), and the crate must link with **no**
//! `_embassy_trace_*` hook symbols — because `metadata-names` pulls in
//! `embassy-executor/metadata-name`, not `embassy-executor/trace`. A real executor
//! drives one `task:` node through `start`, proving the generated `stamp_name` glue
//! compiles and runs. (If this test is ever built with `trace` also on, the glue
//! takes the `adopt` path instead; the assertions below hold either way.)

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

static RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Plain worker; the graph stamps its `#[embassy_executor::task]` shell. The shell's
/// generated glue calls `stamp_name(&token)` before spawning under `metadata-names`.
async fn worker(node: &'static TaskNode) {
    RUNS.fetch_add(1, Ordering::SeqCst);
    node.wait_shutdown().await;
    node.ack_dropped();
}

supervisor_graph! {
    node NAMED = Terminate, deps: [], task: worker;
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
    // start() spawns NAMED through the name-only glue path; if this links and runs,
    // the decoupling holds (no trace recorders, no `_embassy_trace_*` symbols).
    sup.start(spawner).await.expect("start");
    settle(|| RUNS.load(Ordering::SeqCst) == 1).await;
    assert_eq!(RUNS.load(Ordering::SeqCst), 1, "named shell ran");
    assert!(NAMED.is_running(), "named node up after start");
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn metadata_names_only_spawns_without_trace_hooks() {
    let _clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "did not complete (runs={})",
            RUNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
