use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Aborted, Resumed, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::MockDriver;

supervisor_graph! {
    node WORKER = Terminate, deps: [], task: worker;
    node PAUSER = Pause, deps: [], task: pauser;
    node LOOPER = Pause, deps: [], task: looper;
}

static WORK: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static OUTCOME: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static ROUNDS: AtomicU32 = AtomicU32::new(0);
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

static P_WORK: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static P_OUT: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static P_RESUMES: AtomicU32 = AtomicU32::new(0);

static L_WORK: Signal<CriticalSectionRawMutex, u32> = Signal::new();
static L_OUT: Signal<CriticalSectionRawMutex, u32> = Signal::new();

async fn worker(node: &'static TaskNode) {
    loop {
        ROUNDS.fetch_add(1, Ordering::SeqCst);
        match node.run_cancellable(WORK.wait()).await {
            Ok(v) => OUTCOME.signal(v),
            Err(Aborted) => {
                OUTCOME.signal(u32::MAX);
                // No cleanup between select and ack: the _acked variant both
                // proves the immediate-return path (shutdown already requested,
                // wait_shutdown's fast path) and completes the handshake.
                let _ = node
                    .run_cancellable_acked(core::future::pending::<()>())
                    .await;
                return;
            }
        }
    }
}

async fn pauser(node: &'static TaskNode) {
    loop {
        match node.run_pausable(P_WORK.wait()).await {
            Ok(v) => P_OUT.signal(v),
            Err(Resumed) => {
                P_RESUMES.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

/// The whole `Pause` protocol in one call. `cycles` is captured `&mut` across
/// park/resume — its monotonic count proves the same instance kept running.
async fn looper(node: &'static TaskNode) {
    let mut cycles = 0u32;
    node.run_pausable_loop(async || {
        let v = L_WORK.wait().await;
        cycles += 1;
        L_OUT.signal(v * 10 + cycles);
    })
    .await
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
    sup.start(&spawner).await.expect("start");
    settle(|| ROUNDS.load(Ordering::SeqCst) == 1).await;

    WORK.signal(7);
    settle(|| OUTCOME.signaled()).await;
    assert_eq!(OUTCOME.try_take(), Some(7), "Ok path carries the output");
    settle(|| ROUNDS.load(Ordering::SeqCst) == 2).await;
    PHASE.store(1, Ordering::SeqCst);

    sup.stop_node(&WORKER)
        .await
        .expect("acked by the combinator");
    assert_eq!(
        OUTCOME.try_take(),
        Some(u32::MAX),
        "stop surfaced as Err(Aborted) inside the body"
    );
    assert!(!WORKER.is_running());
    assert_eq!(
        ROUNDS.load(Ordering::SeqCst),
        2,
        "no extra round after the abort"
    );
    PHASE.store(2, Ordering::SeqCst);

    P_WORK.signal(5);
    settle(|| P_OUT.signaled()).await;
    assert_eq!(P_OUT.try_take(), Some(5), "Ok path before any pause");

    for round in 1..=2u32 {
        sup.stop_node(&PAUSER)
            .await
            .expect("the combinator acked the pause");
        assert!(!PAUSER.is_running());
        assert_eq!(
            P_RESUMES.load(Ordering::SeqCst),
            round - 1,
            "parked: Err(Resumed) must not surface before the resume"
        );

        sup.resume_node(&PAUSER);
        settle(|| P_RESUMES.load(Ordering::SeqCst) == round).await;
        assert_eq!(P_RESUMES.load(Ordering::SeqCst), round, "resume surfaced");
        assert!(PAUSER.is_running());

        P_WORK.signal(10 + round);
        settle(|| P_OUT.signaled()).await;
        assert_eq!(P_OUT.try_take(), Some(10 + round));
    }
    PHASE.store(3, Ordering::SeqCst);

    sup.resume_pausable();
    sup.stop_node(&PAUSER)
        .await
        .expect("acked by the combinator");
    assert!(!PAUSER.is_running());
    for _ in 0..100 {
        embassy_futures::yield_now().await;
    }
    assert_eq!(
        P_RESUMES.load(Ordering::SeqCst),
        2,
        "a stale resume latch surfaced as a spurious Err(Resumed)"
    );
    sup.resume_pausable();
    settle(|| P_RESUMES.load(Ordering::SeqCst) == 3).await;
    assert!(PAUSER.is_running());
    PHASE.store(4, Ordering::SeqCst);

    L_WORK.signal(3);
    settle(|| L_OUT.signaled()).await;
    assert_eq!(L_OUT.try_take(), Some(31), "cycle 1 before the pause");

    sup.stop_node(&LOOPER).await.expect("acked inside the loop");
    assert!(!LOOPER.is_running());
    sup.resume_node(&LOOPER);

    L_WORK.signal(4);
    settle(|| L_OUT.signaled()).await;
    assert_eq!(
        L_OUT.try_take(),
        Some(42),
        "cycle 2 after the resume: same instance, captured state intact"
    );

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn run_cancellable_races_and_acks() {
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
            "did not complete (phase={}, rounds={}, p_resumes={})",
            PHASE.load(Ordering::SeqCst),
            ROUNDS.load(Ordering::SeqCst),
            P_RESUMES.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
