//! The other half of a demand-started signal: the producer retires once the
//! last reader has left and a cooldown has passed, through the same mailbox
//! the first reader started it with.
//!
//! Readers are driven from the test thread through per-reader command cells;
//! the cooldown runs on the mock clock so every wait is scripted, never timed.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    Backed, ControlOp, Supervisor, TaskNode, dataflow, request_control, supervisor_graph,
};
use embassy_time::{Duration, MockDriver};

pub static EST: Backed<AtomicU32> = Backed::new(AtomicU32::new(0));

const COOLDOWN: Duration = Duration::from_millis(500);

static SPAWNS: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

// Reader script: 0 idle, 1 open requested, 2 holding, 3 drop requested.
const IDLE: u8 = 0;
const OPEN: u8 = 1;
const HOLDING: u8 = 2;
const DROP: u8 = 3;
static CMD: [AtomicU8; 2] = [AtomicU8::new(IDLE), AtomicU8::new(IDLE)];

// Producer script for the hand-rolled handshake window: 0 retire on its own,
// 1 take the manual branch and serve, 2 withdraw readiness and hold, 3
// request the stop.
static PROD_CMD: AtomicU8 = AtomicU8::new(0);
static IN_WINDOW: AtomicBool = AtomicBool::new(false);

supervisor_graph! {
    node PRODUCER = Terminate, deps: [], task: producer, disabled, discover;
    node A = Terminate, deps: [], task: reader_a, dataflow: [crate::reader];
    node B = Terminate, deps: [], task: reader_b, dataflow: [crate::reader];
}

async fn until(cond: impl Fn() -> bool) {
    while !cond() {
        embassy_futures::yield_now().await;
    }
}

#[dataflow]
async fn producer(node: &'static TaskNode) {
    SPAWNS.fetch_add(1, Ordering::SeqCst);
    node.writer(&crate::EST).store(42, Ordering::SeqCst);
    node.set_ready();
    if PROD_CMD.load(Ordering::SeqCst) != 1 {
        // The real thing: retire after the cooldown, or stop when told to.
        let _ = embassy_futures::select::select(
            node.wait_shutdown(),
            node.retire(&crate::EST, COOLDOWN),
        )
        .await;
    } else {
        // The handshake, spelled out, so the test can park inside the window
        // between "readiness withdrawn" and "stop landed".
        until(|| PROD_CMD.load(Ordering::SeqCst) == 2).await;
        node.clear_ready();
        IN_WINDOW.store(true, Ordering::SeqCst);
        until(|| PROD_CMD.load(Ordering::SeqCst) == 3).await;
        request_control(node, ControlOp::Deactivate).await;
    }
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

#[dataflow]
async fn reader(node: &'static TaskNode, i: usize) {
    loop {
        until(|| CMD[i].load(Ordering::SeqCst) == OPEN).await;
        let guard = node.open(&crate::EST).await;
        assert_eq!(
            guard.load(Ordering::SeqCst),
            42,
            "served by a ready producer"
        );
        CMD[i].store(HOLDING, Ordering::SeqCst);
        until(|| CMD[i].load(Ordering::SeqCst) == DROP).await;
        drop(guard);
        CMD[i].store(IDLE, Ordering::SeqCst);
    }
}

async fn reader_a(node: &'static TaskNode) {
    reader(node, 0).await;
}

async fn reader_b(node: &'static TaskNode) {
    reader(node, 1).await;
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    DONE.store(true, Ordering::SeqCst);
    let fault = sup.run(&spawner).await;
    panic!("driver returned: {fault}");
}

#[test]
fn the_producer_retires_after_the_last_reader_leaves() {
    let clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(runner(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(20);
    let wait_for = |what: &str, cond: &dyn Fn() -> bool| {
        while !cond() {
            assert!(
                StdInstant::now() < deadline,
                "{what} did not resolve (spawns={}, openers={}, running={}, disabled={})",
                SPAWNS.load(Ordering::SeqCst),
                EST.openers(),
                PRODUCER.is_running(),
                PRODUCER.is_disabled(),
            );
            std::thread::sleep(StdDuration::from_millis(2));
        }
    };
    let spawns = || SPAWNS.load(Ordering::SeqCst);
    let cmd = |i: usize, c: u8| CMD[i].store(c, Ordering::SeqCst);
    let reader_is = |i: usize, c: u8| move || CMD[i].load(Ordering::SeqCst) == c;
    wait_for("bring-up", &|| DONE.load(Ordering::SeqCst));
    assert!(!PRODUCER.is_running(), "disabled until a reader opens it");

    // ── two readers, one start, the count follows the guards ────────────
    cmd(0, OPEN);
    wait_for("A opened", &reader_is(0, HOLDING));
    assert_eq!(spawns(), 1);
    assert_eq!(EST.openers(), 1);
    cmd(1, OPEN);
    wait_for("B opened", &reader_is(1, HOLDING));
    assert_eq!(spawns(), 1, "the second opener joins a running producer");
    assert_eq!(EST.openers(), 2);
    cmd(0, DROP);
    wait_for("A dropped", &reader_is(0, IDLE));
    assert_eq!(EST.openers(), 1);
    assert!(PRODUCER.is_running(), "one reader left: still serving");
    cmd(1, DROP);
    wait_for("B dropped", &reader_is(1, IDLE));
    assert_eq!(EST.openers(), 0);

    // ── the cooldown is hysteresis: a reader inside it cancels the stop ──
    clock.advance(Duration::from_millis(200));
    std::thread::sleep(StdDuration::from_millis(20));
    assert!(PRODUCER.is_running(), "200 ms into a 500 ms cooldown");
    cmd(0, OPEN);
    wait_for("A reopened inside the cooldown", &reader_is(0, HOLDING));
    assert_eq!(spawns(), 1, "served by the still-running producer");
    clock.advance(Duration::from_secs(2));
    std::thread::sleep(StdDuration::from_millis(20));
    assert!(
        PRODUCER.is_running() && PRODUCER.is_ready(),
        "held: never retires"
    );
    cmd(0, DROP);
    wait_for("A dropped again", &reader_is(0, IDLE));

    // ── nobody for a whole cooldown: the producer stops itself ──────────
    clock.advance(Duration::from_millis(200));
    std::thread::sleep(StdDuration::from_millis(20));
    assert!(PRODUCER.is_running(), "the flap restarted the cooldown");
    clock.advance(Duration::from_millis(400));
    wait_for("retirement", &|| !PRODUCER.is_running());
    assert!(
        PRODUCER.is_disabled(),
        "retired through Deactivate: latched down"
    );
    assert_eq!(spawns(), 1);

    // ── the next reader starts it again ─────────────────────────────────
    PROD_CMD.store(1, Ordering::SeqCst); // the respawn follows the hand-rolled script
    cmd(1, OPEN);
    wait_for("B reopened after retirement", &reader_is(1, HOLDING));
    assert_eq!(spawns(), 2, "a fresh start");
    assert!(!PRODUCER.is_disabled());
    cmd(1, DROP);
    wait_for("B dropped", &reader_is(1, IDLE));

    // ── a reader arriving between clear_ready and the stop waits it out ─
    // The producer has withdrawn readiness but not stopped yet. An opener
    // admitted now must not read a producer on its way out; it waits, and
    // when the stop lands the stop-side wake (not the 250 ms retry, which
    // never fires on a frozen mock clock) lets it request the next start.
    PROD_CMD.store(2, Ordering::SeqCst);
    wait_for("the window", &|| IN_WINDOW.load(Ordering::SeqCst));
    assert!(PRODUCER.is_running() && !PRODUCER.is_ready());
    cmd(0, OPEN);
    std::thread::sleep(StdDuration::from_millis(30));
    assert_eq!(CMD[0].load(Ordering::SeqCst), OPEN, "parked: not serving");
    assert_eq!(
        EST.openers(),
        1,
        "but admitted, so a retiring producer would see it"
    );
    PROD_CMD.store(3, Ordering::SeqCst);
    // (The respawn reads a command other than 1 at spawn: back to auto.)
    wait_for("A served by the respawn", &reader_is(0, HOLDING));
    assert_eq!(
        spawns(),
        3,
        "the stop woke the opener, which requested the start"
    );
    cmd(0, DROP);
    wait_for("A dropped", &reader_is(0, IDLE));
}
