use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, compose_graph, supervisor_fragment};

static NET_SPAWNS: AtomicU32 = AtomicU32::new(0);
static HTTP_SPAWNS: AtomicU32 = AtomicU32::new(0);
static APP_SPAWNS: AtomicU32 = AtomicU32::new(0);
static ONESHOT_RUNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

pub async fn oneshot_worker(_node: &'static TaskNode) {
    ONESHOT_RUNS.fetch_add(1, Ordering::SeqCst);
}

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

supervisor_fragment! {
    name: NET_FRAG;
    node NET = Terminate, deps: [], task: $crate::net_worker;
    node ONESHOT = Terminate, deps: [], task: $crate::oneshot_worker, pool_size: 2;
}

supervisor_fragment! {
    name: HTTP_FRAG;
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
    sup.start(&spawner).await.expect("start");
    settle(|| APP_SPAWNS.load(Ordering::SeqCst) == 1).await;
    assert!(NET.is_running() && HTTP.is_running() && APP.is_running());

    settle(|| ONESHOT_RUNS.load(Ordering::SeqCst) == 1).await;
    settle(|| !ONESHOT.is_running()).await;
    assert!(
        ONESHOT.has_exited(),
        "composed shell recorded the clean return"
    );

    let pos = |name: &str| {
        GRAPH
            .order()
            .position(|i| GRAPH.nodes[i as usize].is_some_and(|n| n.name() == name))
            .unwrap()
    };
    assert!(pos("net") < pos("http") && pos("http") < pos("app"));

    sup.teardown().await.expect("teardown");
    assert!(!NET.is_running() && !HTTP.is_running() && !APP.is_running());
    sup.respawn_terminate(&spawner).await.expect("respawn");
    settle(|| APP_SPAWNS.load(Ordering::SeqCst) == 2).await;
    assert_eq!(NET_SPAWNS.load(Ordering::SeqCst), 2);
    assert_eq!(HTTP_SPAWNS.load(Ordering::SeqCst), 2);

    settle(|| ONESHOT_RUNS.load(Ordering::SeqCst) == 2).await;
    settle(|| !ONESHOT.is_running()).await;
    assert!(ONESHOT.has_exited());

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn fragments_compose_into_one_graph() {
    let _clock = embassy_time::MockDriver::get();

    #[cfg(feature = "data-deps")]
    {
        let seen: Vec<&str> = APP.graph().iter().flatten().map(|n| n.name()).collect();
        assert_eq!(seen.len(), GRAPH.nodes.len(), "{seen:?}");
        assert!(seen.contains(&"app") && seen.contains(&"net"), "{seen:?}");
    }

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
