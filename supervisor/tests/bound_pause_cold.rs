//! A Pause node behind a bound edge, parked at bring-up before it ever ran:
//! the provider's assertion spawns it through the gates (never a "resume"
//! into no task), and a later bound flap resumes the parked instance in place.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

supervisor_graph! {
    node LINK = Terminate, task: link;
    node VIEW = Pause, deps: [LINK ready bound], task: view, slot_timeout: 300;
}

static LINK_READY: AtomicBool = AtomicBool::new(false);
static VIEW_SPAWNS: AtomicU32 = AtomicU32::new(0);
static VIEW_RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn link(node: &'static TaskNode) {
    if LINK_READY.load(Ordering::SeqCst) {
        node.set_ready();
    }
    node.wait_shutdown().await;
    node.mark_exited();
}

async fn view(node: &'static TaskNode) {
    VIEW_SPAWNS.fetch_add(1, Ordering::SeqCst);
    loop {
        VIEW_RUNS.fetch_add(1, Ordering::SeqCst);
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
    }
}

async fn settle(sup: &Supervisor<2, GRAPH_TOPOLOGY>, spawner: Spawner) {
    sup.apply_bind(&spawner).await.expect("cascade");
    for _ in 0..8 {
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner)
        .await
        .expect("a bound edge defers, not faults");
    assert!(VIEW.is_bound_stopped() && !VIEW.is_running());
    assert_eq!(
        VIEW_SPAWNS.load(Ordering::SeqCst),
        0,
        "parked, never spawned"
    );

    LINK_READY.store(true, Ordering::SeqCst);
    LINK.set_ready();
    settle(&sup, spawner).await;
    assert_eq!(
        VIEW_SPAWNS.load(Ordering::SeqCst),
        1,
        "spawned through the gates, not resumed into no task"
    );
    assert!(VIEW.is_running() && !VIEW.is_bound_stopped());

    LINK.clear_ready();
    settle(&sup, spawner).await;
    assert!(
        !VIEW.is_running() && VIEW.is_bound_stopped(),
        "bound-stopped: parked"
    );
    LINK.set_ready();
    settle(&sup, spawner).await;
    assert!(VIEW.is_running());
    assert_eq!(
        VIEW_SPAWNS.load(Ordering::SeqCst),
        1,
        "resumed in place, not respawned"
    );
    assert_eq!(VIEW_RUNS.load(Ordering::SeqCst), 2);
    sup.stop_node(&VIEW)
        .await
        .expect("a real instance acks its pause");
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_cold_pause_node_behind_a_bound_edge_is_spawned_not_resumed() {
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
        assert!(StdInstant::now() < deadline, "did not complete");
        clock.advance(embassy_time::Duration::from_millis(100));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
