//! `fault-inject`: each verb is done TO the task. Stall withholds the shell's
//! polls, wedge hides the request and swallows the ack, crash drops the
//! worker, hog busy-spins the executor. Nothing in the workers cooperates.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{
    Fault, FaultKind, HealthKind, InjectError, Supervisor, TaskNode, supervisor_graph,
    try_wait_health,
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, MockDriver, Timer};

static STALLER_TICKS: AtomicU32 = AtomicU32::new(0);
static SIBLING_TICKS: AtomicU32 = AtomicU32::new(0);
static HOGGER_TICKS: AtomicU32 = AtomicU32::new(0);
static EARLY_GO: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static DROPPER_GO: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static LEAVER_GO: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static DONE: AtomicBool = AtomicBool::new(false);

/// Beats every 20 ms and counts; answers a stop through `wait_shutdown`.
async fn ticking(node: &'static TaskNode, ticks: &AtomicU32) {
    loop {
        match select(
            Timer::after(Duration::from_millis(20)),
            node.wait_shutdown(),
        )
        .await
        {
            Either::First(()) => {
                ticks.fetch_add(1, Ordering::Release);
                node.beat();
            }
            Either::Second(()) => {
                node.ack_dropped();
                return;
            }
        }
    }
}
async fn staller_worker(node: &'static TaskNode) {
    ticking(node, &STALLER_TICKS).await
}
async fn sibling_worker(node: &'static TaskNode) {
    ticking(node, &SIBLING_TICKS).await
}
async fn hogger_worker(node: &'static TaskNode) {
    ticking(node, &HOGGER_TICKS).await
}

/// The cancel-driver path: acks through `run_cancellable_acked`.
async fn cooperative_worker(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// Returns on its own when told: the shell's `mark_exited` is the ack path.
async fn early_worker(_node: &'static TaskNode) {
    EARLY_GO.wait().await;
}

/// The hand-rolled Pause shape: ack, park, resume.
async fn pausable_worker(node: &'static TaskNode) {
    loop {
        node.wait_shutdown().await;
        node.ack_dropped();
        node.wait_resume().await;
    }
}

/// Lends a resource and would return a value; a crash gives neither a value
/// nor a leak.
async fn crasher_worker(_node: &'static TaskNode, lent: &mut u32) -> u32 {
    *lent += 1;
    core::future::pending::<()>().await;
    0
}

/// Acks unconditionally when told, then stays alive: the ack, not the exit,
/// is what a wedge swallows here, and the worker is still around when the
/// wedge is replaced.
async fn dropper_worker(node: &'static TaskNode) {
    DROPPER_GO.wait().await;
    node.ack_dropped();
    core::future::pending::<()>().await
}

#[embassy_executor::task]
async fn handwritten(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// A hand-written task that returns on its own when told: no shell, its own
/// `mark_exited` is the ack path.
#[embassy_executor::task]
async fn leaver(node: &'static TaskNode) {
    LEAVER_GO.wait().await;
    node.mark_exited();
}

supervisor_graph! {
    node STALLER = Terminate, deps: [], task: staller_worker, beat_timeout: 100;
    node SIBLING = Terminate, deps: [], task: sibling_worker, beat_timeout: 100;
    node HOGGER = Terminate, deps: [], task: hogger_worker, beat_timeout: 100;
    node WEDGER = Terminate, deps: [], task: cooperative_worker, ack_timeout: 100;
    node EARLY = Terminate, deps: [], task: early_worker, ack_timeout: 100;
    node PAUSER = Pause, deps: [], task: pausable_worker, ack_timeout: 100;
    node CRASHER = Terminate, deps: [], task: crasher_worker, exit: u32,
        resources: [LENT: u32];
    node DROPPER = Terminate, deps: [], task: dropper_worker, ack_timeout: 100;
    node HAND = Terminate, deps: [], spawn: handwritten;
    node LEAVER = Terminate, deps: [], spawn: leaver, ack_timeout: 100;
}

static SUP: Supervisor<10, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);

#[embassy_executor::task]
async fn monitor_task() {
    SUP.monitor().await;
}

/// Let mock time pass (the test thread advances the clock) while yielding.
async fn pass(ms: u64) {
    let t0 = Instant::now();
    while Instant::now() - t0 < Duration::from_millis(ms) {
        embassy_futures::yield_now().await;
    }
}

/// Yield for `ms` of mock time and return the largest jump seen between two
/// consecutive yields: how long this executor went without polling anything.
async fn max_gap(ms: u64) -> Duration {
    let t0 = Instant::now();
    let mut prev = t0;
    let mut gap = Duration::from_ticks(0);
    while Instant::now() - t0 < Duration::from_millis(ms) {
        embassy_futures::yield_now().await;
        let now = Instant::now();
        gap = gap.max(now - prev);
        prev = now;
    }
    gap
}

async fn settle(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..20_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
    panic!("never held: {what}");
}

fn drain() -> Vec<(&'static str, HealthKind)> {
    let mut out = Vec::new();
    while let Some(ev) = try_wait_health() {
        out.push((ev.node.name(), ev.kind));
    }
    out
}

fn count(events: &[(&'static str, HealthKind)], name: &str, stale: bool) -> usize {
    events
        .iter()
        .filter(|(n, k)| *n == name && matches!(k, HealthKind::Stale { .. }) == stale)
        .count()
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    LENT.provide(40);
    spawner.spawn(monitor_task().unwrap());
    SUP.start(&spawner).await.expect("bring-up");
    settle("#1", || STALLER.is_running() && HAND.is_running()).await;
    pass(300).await;
    assert!(drain().is_empty(), "healthy graph, no events");

    // ── stall: the worker is not polled ────────────────────────────────────
    assert!(STALLER.fault() == Fault::None);
    STALLER
        .inject(Fault::Stall)
        .expect("a task: node has a shell");
    pass(50).await;
    let frozen = STALLER_TICKS.load(Ordering::Acquire);
    let sibling_before = SIBLING_TICKS.load(Ordering::Acquire);
    pass(400).await;
    assert_eq!(
        STALLER_TICKS.load(Ordering::Acquire),
        frozen,
        "no poll, no tick"
    );
    assert!(
        SIBLING_TICKS.load(Ordering::Acquire) > sibling_before,
        "only the stalled node froze"
    );
    let events = drain();
    assert_eq!(
        count(&events, "staller", true),
        1,
        "the monitor reports it stale once: {events:?}"
    );
    assert_eq!(count(&events, "sibling", true), 0);
    assert!(STALLER.is_running(), "stalled, not down");

    STALLER.clear_fault();
    pass(300).await;
    assert!(
        STALLER_TICKS.load(Ordering::Acquire) > frozen,
        "polled again after clear"
    );
    let events = drain();
    assert_eq!(count(&events, "staller", false), 1, "recovered: {events:?}");

    // A stalled task still answers a stop, like a real stall parked on a select.
    STALLER.inject(Fault::Stall).unwrap();
    pass(50).await;
    SUP.stop_node(&STALLER)
        .await
        .expect("the stall lifts for the shutdown");
    assert!(!STALLER.is_running());
    assert_eq!(
        STALLER.fault(),
        Fault::Stall,
        "a stop does not clear the injected fault"
    );
    STALLER.clear_fault();

    // ── wedge: the cooperative worker never sees the request ───────────────
    WEDGER.inject(Fault::Wedge).unwrap();
    let err = SUP
        .stop_node(&WEDGER)
        .await
        .expect_err("no ack inside the window");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout), "{err}");
    assert!(
        WEDGER.is_running(),
        "still marked running after the timeout"
    );
    assert!(!WEDGER.shutdown_requested(), "the request is hidden");
    WEDGER.clear_fault();
    settle("#2", || !WEDGER.is_running()).await;
    assert!(
        WEDGER.shutdown_requested(),
        "and visible again once cleared"
    );

    // ── wedge: a worker that returns on its own is swallowed too ───────────
    EARLY.inject(Fault::Wedge).unwrap();
    EARLY_GO.signal(());
    pass(50).await;
    assert!(
        EARLY.is_running() && !EARLY.has_exited(),
        "the exit was swallowed"
    );
    let err = SUP
        .stop_node(&EARLY)
        .await
        .expect_err("nothing acks a returned worker while wedged");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout), "{err}");
    // The worker is gone: the clear itself delivers the swallowed exit.
    EARLY.clear_fault();
    settle("#3", || !EARLY.is_running()).await;
    assert!(EARLY.has_exited(), "the swallowed exit landed on the clear");

    // ── wedge across a pause, then a resume in place ───────────────────────
    PAUSER.inject(Fault::Wedge).unwrap();
    let err = SUP
        .deactivate(&PAUSER)
        .await
        .expect_err("the pause never acks");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout), "{err}");
    PAUSER.clear_fault();
    settle("#4", || !PAUSER.is_running()).await;
    SUP.activate(&PAUSER, &spawner).await;
    settle("#5", || PAUSER.is_running()).await;
    SUP.deactivate(&PAUSER)
        .await
        .expect("a clean pause after the episode");
    assert!(!PAUSER.is_running());

    // ── crash: the future is dropped, the shell exits cleanly ──────────────
    CRASHER.inject(Fault::Crash).unwrap();
    settle("#6", || CRASHER.has_exited()).await;
    assert!(!CRASHER.is_running());
    assert_eq!(CRASHER.fault(), Fault::None, "a crash is one-shot");
    assert!(
        CRASHER_EXIT.take().is_none(),
        "no return value from a crash"
    );
    assert_eq!(
        LENT.take(),
        Some(41),
        "the lent resource came back, mutated by the worker"
    );
    LENT.restore(41);
    SUP.restart(&CRASHER, &spawner)
        .await
        .expect("a crashed node respawns");
    settle("#7", || CRASHER.is_running()).await;

    // ── hog: the whole executor freezes with it ────────────────────────────
    let quiet = max_gap(300).await;
    assert!(
        quiet < Duration::from_millis(100),
        "control: no freeze without a hog ({quiet})"
    );
    HOGGER
        .inject(Fault::Hog(Duration::from_millis(400)))
        .unwrap();
    // The grace, then the spin: this driver, the sibling and the monitor are
    // all on the hogged executor, so none of them is polled for 400 ms of mock
    // time. The driver sees that as one jump between two yields.
    let frozen = max_gap(1_000).await;
    assert!(
        frozen >= Duration::from_millis(400),
        "the executor froze for the bound ({frozen})"
    );
    assert_eq!(
        HOGGER.fault(),
        Fault::None,
        "a hog clears itself at its bound"
    );
    // The monitor's sweep is frozen with everything else, so its stale report
    // is sweep-order-dependent; the freeze and the self-clear are the facts.
    let _ = drain();
    let sibling_before = SIBLING_TICKS.load(Ordering::Acquire);
    pass(200).await;
    assert!(
        SIBLING_TICKS.load(Ordering::Acquire) > sibling_before,
        "everything resumes"
    );

    // ── replacing a wedge with another fault does not strand a swallowed ack ──
    // The worker acks while wedged (swallowed) and stays alive; the wedge is
    // then replaced rather than cleared. The replacement delivers the ack: a
    // wedge is the only fault that withholds one.
    DROPPER.inject(Fault::Wedge).unwrap();
    DROPPER_GO.signal(());
    pass(50).await;
    assert!(DROPPER.is_running(), "the ack was swallowed");
    let err = SUP.stop_node(&DROPPER).await.expect_err("wedged, no ack");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout), "{err}");
    DROPPER.inject(Fault::Stall).unwrap();
    settle("dropper acks once the wedge is replaced", || {
        !DROPPER.is_running()
    })
    .await;
    DROPPER.clear_fault();
    pass(50).await;
    assert!(!DROPPER.is_running(), "nothing left to replay");

    // ── a hand-written spawn: task has no shell ────────────────────────────
    assert_eq!(HAND.inject(Fault::Stall), Err(InjectError::NoShell));
    assert_eq!(HAND.inject(Fault::Crash), Err(InjectError::NoShell));
    assert_eq!(
        HAND.inject(Fault::Hog(Duration::from_millis(1))),
        Err(InjectError::NoShell)
    );
    HAND.inject(Fault::Wedge).expect("wedge lives in the node");
    let err = SUP.stop_node(&HAND).await.expect_err("wedged");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout), "{err}");
    HAND.clear_fault();
    settle("#8", || !HAND.is_running()).await;

    // ── a hand-written task that returns while wedged ─────────────────────
    // No shell will ever poll on its behalf: the clear delivers the exit.
    LEAVER.inject(Fault::Wedge).unwrap();
    LEAVER_GO.signal(());
    pass(50).await;
    assert!(
        LEAVER.is_running() && !LEAVER.has_exited(),
        "the exit was swallowed"
    );
    let err = SUP.stop_node(&LEAVER).await.expect_err("wedged, no ack");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout), "{err}");
    LEAVER.clear_fault();
    settle("#9", || !LEAVER.is_running()).await;
    assert!(
        LEAVER.has_exited(),
        "the swallowed exit landed on the clear"
    );

    DONE.store(true, Ordering::Release);
}

#[test]
fn faults_are_done_to_the_task() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    // Mock time runs from here: 10 ms per 2 ms of wall time, so windows and
    // the hog's spin resolve without the executor thread advancing anything.
    let deadline = StdInstant::now() + StdDuration::from_secs(60);
    while !DONE.load(Ordering::Acquire) {
        clock.advance(Duration::from_millis(10));
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
