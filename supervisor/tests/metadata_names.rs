use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

/// The executor's handle on a task's metadata: `Metadata` in embassy-executor
/// 0.10, `MetadataRef` in the post-0.10 API that `embassy_supervisor_trace_v2`
/// selects (the trace canary builds against it).
#[cfg(not(embassy_supervisor_trace_v2))]
use embassy_executor::Metadata as TaskMetadata;
#[cfg(embassy_supervisor_trace_v2)]
use embassy_executor::MetadataRef as TaskMetadata;

static RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);
/// The name the running shell finds in its own task Metadata.
static STAMPED: Mutex<Option<&'static str>> = Mutex::new(None);

async fn worker(node: &'static TaskNode) {
    let name = TaskMetadata::for_current_task().await.name();
    *STAMPED.lock().unwrap() = name;
    RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
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
    assert!(f(), "did not settle");
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    // start() spawns NAMED through the name-only glue (`stamp_name`; with
    // `trace` on, `adopt` stamps the same name).
    sup.start(&spawner).await.expect("start");
    settle(|| RUNS.load(Ordering::SeqCst) == 1).await;
    assert!(NAMED.is_running(), "named node up after start");
    assert_eq!(
        *STAMPED.lock().unwrap(),
        Some("named\0"),
        "metadata carries the NUL-terminated name"
    );
    assert_eq!(NAMED.name(), "named", "name() keeps the plain text");
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
