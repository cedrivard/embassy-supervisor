//! `apply_control`'s Restart arm against the verb it wraps: a startup fault on
//! the way back up is returned by `restart` but logged and absorbed by the
//! control path, while a missed shutdown ack is returned by both.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{
    ControlCommand, ControlOp, FaultKind, Supervisor, TaskNode, supervisor_graph,
};
use embassy_time::Duration;

supervisor_graph! {
    node SOURCE = Terminate, deps: [], task: worker;
    node SINK = Terminate, deps: [SOURCE], task: sink, resources: [TOKEN: consume u32];
    node DEAF = Terminate, deps: [], task: deaf, ack_timeout: 100;
}

/// Set around the calls that wait out a gate or an ack window.
static ADVANCE: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn worker(node: &'static TaskNode) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// Consumes its token: once it stops, nothing refills TOKEN.
async fn sink(node: &'static TaskNode, _token: u32) {
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

/// Never acks.
async fn deaf(_node: &'static TaskNode) {
    core::future::pending::<()>().await;
}

fn restart_cmd(node: &'static TaskNode) -> ControlCommand {
    ControlCommand {
        node,
        op: ControlOp::Restart,
    }
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    TOKEN.provide(7);
    sup.start(&spawner).await.expect("start");

    ADVANCE.store(true, Ordering::SeqCst);
    let err = sup
        .restart(&SOURCE, &spawner)
        .await
        .expect_err("SINK's consumed token is gone");
    ADVANCE.store(false, Ordering::SeqCst);
    assert!(
        core::ptr::eq(err.node, &SINK) && matches!(err.kind, FaultKind::ResourceMissing),
        "the verb returns the startup fault: {err}"
    );

    ADVANCE.store(true, Ordering::SeqCst);
    sup.apply_control(restart_cmd(&SOURCE), &spawner)
        .await
        .expect("the control path logs the same startup fault and absorbs it");
    ADVANCE.store(false, Ordering::SeqCst);
    assert!(
        SOURCE.is_running() && !SINK.is_running(),
        "the target came back, its dependent stayed down"
    );

    ADVANCE.store(true, Ordering::SeqCst);
    let err = sup
        .apply_control(restart_cmd(&DEAF), &spawner)
        .await
        .expect_err("a missed ack is returned, not absorbed");
    ADVANCE.store(false, Ordering::SeqCst);
    assert!(
        core::ptr::eq(err.node, &DEAF) && matches!(err.kind, FaultKind::ShutdownTimeout),
        "{err}"
    );
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn restart_absorbs_startup_faults_and_returns_missed_acks() {
    support::run(
        |spawner| spawner.spawn(driver(spawner).unwrap()),
        &DONE,
        support::Clock::While(&ADVANCE, Duration::from_millis(10)),
        10,
        || "did not complete".to_owned(),
    );
}
