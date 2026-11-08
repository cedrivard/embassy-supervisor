//! The lifecycle queue around the waves, read through the awaiting consumer: a
//! body that returns unasked is not evented, teardown events each stop in
//! dependency order, `start()` re-entry and `resume_pausable` report `Resumed`,
//! a gate timeout is `Faulted` on the single-node verb and the start wave alike,
//! and a missed ack is the stop wave's `Faulted`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    FaultKind, LifecycleKind, Supervisor, TaskNode, lifecycle_events_dropped, supervisor_graph,
    try_wait_lifecycle, wait_lifecycle,
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node PARK = Pause, deps: [], task: park;
    node LEAF = Terminate, deps: [PARK], task: leaf;
    node SOLO = Terminate, deps: [], task: solo;
    node GATED = Terminate, deps: [], task: gated, disabled, resources: [SLOT: u32];
    node DEAF = Terminate, deps: [], task: deaf, disabled, ack_timeout: 200;
}

static SOLO_GO: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Set around the phases that wait out a gate or an ack window.
static ADVANCE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn park(node: &'static TaskNode) {
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
    }
}

async fn leaf(node: &'static TaskNode) {
    node.wait_shutdown().await;
    node.mark_exited();
}

/// Returns on its own once told to, or on a stop.
async fn solo(node: &'static TaskNode) {
    let _ = embassy_futures::select::select(SOLO_GO.wait(), node.wait_shutdown()).await;
}

async fn gated(node: &'static TaskNode, _slot: &mut u32) {
    node.wait_shutdown().await;
    node.mark_exited();
}

/// Never acks.
async fn deaf(_node: &'static TaskNode) {
    core::future::pending::<()>().await;
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

fn drain() -> Vec<(&'static str, LifecycleKind)> {
    let mut out = Vec::new();
    while let Some(ev) = try_wait_lifecycle() {
        out.push((ev.node.name(), ev.kind));
    }
    out
}

fn names(events: &[(&'static str, LifecycleKind)]) -> Vec<(&'static str, &'static str)> {
    events.iter().map(|(n, k)| (*n, k.as_str())).collect()
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);

    sup.start(&spawner).await.expect("start");
    let mut started = Vec::new();
    for _ in 0..3 {
        let ev = wait_lifecycle().await;
        assert!(
            matches!(ev.kind, LifecycleKind::Started),
            "{}",
            ev.kind.as_str()
        );
        started.push(ev.node.name());
    }
    let at = |name: &str| {
        started
            .iter()
            .position(|n| *n == name)
            .unwrap_or_else(|| panic!("no Started for {name}: {started:?}"))
    };
    assert!(
        at("park") < at("leaf"),
        "the awaiting consumer gets the wave in dependency order"
    );
    at("solo");
    assert!(
        try_wait_lifecycle().is_none(),
        "GATED and DEAF are disabled: three spawns, three events"
    );

    SOLO_GO.signal(());
    settle(|| SOLO.has_exited()).await;
    assert!(
        try_wait_lifecycle().is_none(),
        "a body that returns unasked is not evented"
    );

    sup.teardown().await.expect("teardown");
    assert_eq!(
        names(&drain()),
        [
            ("leaf", "stop_requested"),
            ("leaf", "exited"),
            ("park", "stop_requested"),
            ("park", "stopped"),
        ],
        "the stop wave events each stop, dependents first; the exited SOLO is not stopped"
    );

    sup.start(&spawner).await.expect("re-entry");
    let events = names(&drain());
    let at = |e: (&'static str, &'static str)| {
        events
            .iter()
            .position(|x| *x == e)
            .unwrap_or_else(|| panic!("no {e:?} in {events:?}"))
    };
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(
        at(("park", "resumed")) < at(("leaf", "started")),
        "re-entry resumes the parked instance before its dependent respawns"
    );
    at(("solo", "started"));

    sup.stop_node(&PARK).await.expect("park parks");
    sup.resume_pausable();
    assert_eq!(
        names(&drain()),
        [
            ("park", "stop_requested"),
            ("park", "stopped"),
            ("park", "resumed")
        ],
        "resume_pausable reports the instance it wakes"
    );

    ADVANCE.store(true, Ordering::SeqCst);
    let err = sup
        .start_node(&GATED, &spawner)
        .await
        .expect_err("SLOT is empty");
    ADVANCE.store(false, Ordering::SeqCst);
    assert!(matches!(err.kind, FaultKind::ResourceMissing), "{err}");
    let events = drain();
    assert_eq!(
        names(&events),
        [("gated", "faulted")],
        "picking a node is not evented; its gate fault is"
    );
    assert!(matches!(
        events[0].1,
        LifecycleKind::Faulted(FaultKind::ResourceMissing)
    ));

    GATED.set_disabled(false);
    ADVANCE.store(true, Ordering::SeqCst);
    let err = sup
        .start(&spawner)
        .await
        .expect_err("the wave's gate times out the same way");
    ADVANCE.store(false, Ordering::SeqCst);
    assert!(core::ptr::eq(err.node, &GATED), "{err}");
    let events = drain();
    assert_eq!(
        names(&events),
        [("gated", "faulted")],
        "the start wave events its fault"
    );
    assert!(matches!(
        events[0].1,
        LifecycleKind::Faulted(FaultKind::ResourceMissing)
    ));

    sup.teardown().await.expect("teardown");
    assert_eq!(
        drain().len(),
        6,
        "LEAF, SOLO and PARK: a request and an ack each"
    );

    sup.start_node(&DEAF, &spawner).await.expect("start DEAF");
    assert_eq!(names(&drain()), [("deaf", "started")]);
    ADVANCE.store(true, Ordering::SeqCst);
    let err = sup.teardown().await.expect_err("DEAF never acks");
    ADVANCE.store(false, Ordering::SeqCst);
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout), "{err}");
    let events = drain();
    assert_eq!(
        names(&events),
        [("deaf", "stop_requested"), ("deaf", "faulted")]
    );
    assert!(
        matches!(
            events[1].1,
            LifecycleKind::Faulted(FaultKind::ShutdownTimeout)
        ),
        "the stop wave's missed ack carries the fault teardown returned"
    );

    assert_eq!(
        lifecycle_events_dropped(),
        0,
        "every phase drained: nothing fell off"
    );
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn the_waves_and_verbs_report_every_transition() {
    let clock = MockDriver::get();
    let thread = std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| spawner.spawn(driver(spawner).unwrap()));
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        if thread.is_finished() {
            thread.join().expect("executor assertions failed");
            panic!("executor exited before completion");
        }
        assert!(StdInstant::now() < deadline, "did not complete");
        if ADVANCE.load(Ordering::SeqCst) {
            clock.advance(Duration::from_millis(10));
        }
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
