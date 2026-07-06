//! Host tests for `trace-nested` charge-splitting (see the `[[test]]` entry in
//! Cargo.toml): a nested higher-tier poll must be relieved from the interrupted
//! window, so the victim's exec/max-poll numbers are preemption-exact and the
//! tier's own numbers stay exact too. One sequential `#[test]` — global state.

use embassy_supervisor::{supervisor_graph, trace};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node LOW = Terminate, deps: [];
    node HIGHER = Terminate, deps: [];
}

/// Two fake executors: the "thread" tier and the preempting "interrupt" tier.
const THREAD: u32 = 0x1000;
const INT: u32 = 0x2000;

#[test]
fn charge_splitting() {
    let clock = MockDriver::get();
    trace::register_graph(&GRAPH.nodes[..]);
    LOW.set_task_id(11);
    HIGHER.set_task_id(22);

    // ── single preemption: LOW runs 100 ticks of real work, INT steals 40 in
    // the middle. Hook order is exactly what one core produces. ─────────────
    trace::on_task_exec_begin(THREAD, 11);
    clock.advance(Duration::from_ticks(30)); // LOW works
    trace::on_task_exec_begin(INT, 22); // preempted
    clock.advance(Duration::from_ticks(40)); // INT works
    trace::on_task_exec_end(INT, 22);
    clock.advance(Duration::from_ticks(70)); // LOW works again
    trace::on_task_exec_end(THREAD, 11);

    assert_eq!(HIGHER.exec_ticks(), 40, "tier's own time exact");
    assert_eq!(HIGHER.max_poll_ticks(), 40);
    assert_eq!(LOW.exec_ticks(), 100, "victim relieved of the stolen 40");
    assert_eq!(LOW.max_poll_ticks(), 100, "watermark not inflated to 140");
    let th = trace::executor_stats(THREAD).unwrap();
    let it = trace::executor_stats(INT).unwrap();
    assert_eq!(th.exec_ticks, 100, "executor-level in-poll corrected too");
    assert_eq!(it.exec_ticks, 40);

    // ── double nesting: a third tier preempts the second ────────────────────
    const TOP: u32 = 0x3000;
    trace::on_task_exec_begin(THREAD, 11);
    clock.advance(Duration::from_ticks(10));
    trace::on_task_exec_begin(INT, 22);
    clock.advance(Duration::from_ticks(5));
    trace::on_task_exec_begin(TOP, 999); // unsupervised top tier
    clock.advance(Duration::from_ticks(20));
    trace::on_task_exec_end(TOP, 999);
    clock.advance(Duration::from_ticks(5));
    trace::on_task_exec_end(INT, 22);
    clock.advance(Duration::from_ticks(10));
    trace::on_task_exec_end(THREAD, 11);

    // LOW: 10 + 10 real work; INT window raw 30 minus TOP's 20 = 10 real;
    // LOW is relieved of INT's full 30-tick wall occupation.
    assert_eq!(LOW.exec_ticks(), 100 + 20, "second poll adds 20 exact");
    assert_eq!(
        HIGHER.exec_ticks(),
        40 + 10,
        "middle tier keeps only its own 10"
    );
    assert_eq!(trace::executor_stats(TOP).unwrap().exec_ticks, 20);
    assert_eq!(
        LOW.max_poll_ticks(),
        100,
        "the 20-tick corrected poll does not beat the watermark"
    );

    // ── preemption over an idle window: idle by definition unaffected ──────
    trace::on_executor_idle(THREAD);
    clock.advance(Duration::from_ticks(50));
    trace::on_task_exec_begin(INT, 22);
    clock.advance(Duration::from_ticks(30));
    trace::on_task_exec_end(INT, 22);
    clock.advance(Duration::from_ticks(20));
    let th = trace::executor_stats(THREAD).unwrap();
    assert_eq!(
        th.idle_ticks, 100,
        "THREAD idle window keeps running across INT's poll (idle = 'this \
         executor not running', which stays true)"
    );
    assert_eq!(
        HIGHER.exec_ticks(),
        40 + 10 + 30,
        "no parent, nothing credited"
    );
}
