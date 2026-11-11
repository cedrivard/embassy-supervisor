mod support;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use support::settle;

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

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    // start() spawns NAMED through the name-only glue (`stamp_name`; with
    // `trace` on, `adopt` stamps the same name).
    sup.start(&spawner).await.expect("start");
    settle("first run", || RUNS.load(Ordering::SeqCst) == 1).await;
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
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || format!("did not complete (runs={})", RUNS.load(Ordering::SeqCst)),
    );
}
