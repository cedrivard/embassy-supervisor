//! Behavioral tests for the lossless control mailbox: `try_request_control`
//! surfaces a full channel as `Err(ControlQueueFull)` instead of silently
//! dropping (the 0.3.x defect), and the awaiting `request_control` parks until
//! the driver drains a command, so delivery is guaranteed. A single real embassy
//! executor on a std thread, as in `teardown.rs`. The mailbox API is independent of
//! `supervisor_graph!`, but a minimal graph invocation (one parked node) stays:
//! under `trace-hooks` it is what emits the `_embassy_trace_*` symbols this test
//! binary links against.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlOp, ControlQueueFull, request_control, supervisor_graph, try_request_control,
    wait_control,
};

supervisor_graph! {
    // Parked (no spawn:) — the mailbox tests only need a &'static TaskNode target.
    node TARGET = Terminate, deps: [];
}

/// Set once the parked awaiting sender has delivered its request.
static AWAITED_SENT: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

/// Parked sender: with the mailbox full this must not complete until the driver
/// drains a slot — awaiting delivery instead of dropping.
#[embassy_executor::task]
async fn awaiting_sender() {
    request_control(&TARGET, ControlOp::Activate).await;
    AWAITED_SENT.store(true, Ordering::SeqCst);
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    // ── try_request_control: exactly 4 fit; the 5th reports full, drops nothing ──
    for i in 0..4 {
        assert!(
            try_request_control(&TARGET, ControlOp::Activate).is_ok(),
            "request {i} fits in the 4-deep mailbox"
        );
    }
    assert_eq!(
        try_request_control(&TARGET, ControlOp::Deactivate),
        Err(ControlQueueFull),
        "5th request reports the full mailbox instead of vanishing"
    );
    PHASE.store(1, Ordering::SeqCst);

    // ── awaiting request_control: parks on the full mailbox ─────────────────
    spawner.spawn(awaiting_sender().unwrap());
    for _ in 0..100 {
        embassy_futures::yield_now().await;
    }
    assert!(
        !AWAITED_SENT.load(Ordering::SeqCst),
        "awaiting sender is parked while the mailbox is full"
    );
    PHASE.store(2, Ordering::SeqCst);

    // ── draining one slot unblocks it; all 5 commands arrive in order ───────
    let first = wait_control().await;
    assert!(core::ptr::eq(first.node, &TARGET));
    assert_eq!(first.op, ControlOp::Activate);
    for _ in 0..100 {
        embassy_futures::yield_now().await;
    }
    assert!(
        AWAITED_SENT.load(Ordering::SeqCst),
        "awaiting sender delivered once a slot freed"
    );
    // 3 remaining try_-sent commands, then the awaited one — nothing was lost.
    for _ in 0..3 {
        assert_eq!(wait_control().await.op, ControlOp::Activate);
    }
    assert_eq!(
        wait_control().await.op,
        ControlOp::Activate,
        "the awaited request is the 5th delivered"
    );
    // Mailbox empty again: try_ succeeds.
    assert!(try_request_control(&TARGET, ControlOp::Deactivate).is_ok());
    assert_eq!(wait_control().await.op, ControlOp::Deactivate);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn control_mailbox_is_lossless() {
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
            "did not complete (phase={}, awaited_sent={})",
            PHASE.load(Ordering::SeqCst),
            AWAITED_SENT.load(Ordering::SeqCst),
        );
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
