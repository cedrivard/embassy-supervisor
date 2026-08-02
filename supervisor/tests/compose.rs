//! End-to-end test of graph composition: two `supervisor_fragment!` relays plus
//! compose-site items assemble into ONE `supervisor_graph!` expansion via
//! `compose_graph!` — cross-fragment deps resolve by name (both directions:
//! the compose site depends on a fragment node, one fragment depends on the
//! other's node), a `shared` slot declared in a fragment and at the compose
//! site dedups into one static, and the composed graph runs a full
//! start -> stop -> respawn lifecycle. Harness as teardown.rs.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, compose_graph, supervisor_fragment};

static NET_SPAWNS: AtomicU32 = AtomicU32::new(0);
static HTTP_SPAWNS: AtomicU32 = AtomicU32::new(0);
static APP_SPAWNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

pub async fn net_worker(node: &'static TaskNode) {
    NET_SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

pub async fn http_worker(node: &'static TaskNode, port: u16) {
    assert_eq!(port, 80, "shared slot fanned into the fragment worker");
    HTTP_SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

async fn app_worker(node: &'static TaskNode, port: u16) {
    assert_eq!(port, 80);
    APP_SPAWNS.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

// In-crate fragments: `$crate` here resolves to this test crate itself — the
// same mechanism a foreign crate's `$crate::…` would use. (True cross-crate
// composition is exercised by the demo firmware; a single-crate test can't
// model two crates.)
supervisor_fragment! {
    name: NET_FRAG;
    node NET = Terminate, deps: [], task: $crate::net_worker;
}

supervisor_fragment! {
    name: HTTP_FRAG;
    // Cross-fragment dep (NET lives in NET_FRAG) + a shared slot this fragment
    // declares; the compose-site item re-declares it (kinds + type verbatim),
    // deduping into one static.
    node HTTP = Terminate, deps: [NET], task: $crate::http_worker,
        resources: [PORT: shared u16];
}

compose_graph! {
    fragments: [NET_FRAG, HTTP_FRAG],
    graph: {
        node APP = Terminate, deps: [HTTP], task: app_worker,
            resources: [PORT: shared u16];
    }
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
    PORT.provide(80);
    sup.start(spawner).await.expect("start");
    settle(|| APP_SPAWNS.load(Ordering::SeqCst) == 1).await;
    assert!(NET.is_running() && HTTP.is_running() && APP.is_running());

    // The composed order is dependency-first across fragment boundaries.
    let pos = |name: &str| {
        GRAPH
            .order
            .iter()
            .position(|&i| GRAPH.nodes[i as usize].is_some_and(|n| n.name == name))
            .unwrap()
    };
    assert!(pos("net") < pos("http") && pos("http") < pos("app"));

    // Full lifecycle across the composed graph.
    sup.teardown().await.expect("teardown");
    assert!(!NET.is_running() && !HTTP.is_running() && !APP.is_running());
    sup.respawn_terminate(spawner).await.expect("respawn");
    settle(|| APP_SPAWNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(NET_SPAWNS.load(Ordering::SeqCst), 2);
    assert_eq!(HTTP_SPAWNS.load(Ordering::SeqCst), 2);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn fragments_compose_into_one_graph() {
    let _clock = embassy_time::MockDriver::get();

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
            "did not complete (net={}, http={}, app={})",
            NET_SPAWNS.load(Ordering::SeqCst),
            HTTP_SPAWNS.load(Ordering::SeqCst),
            APP_SPAWNS.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
