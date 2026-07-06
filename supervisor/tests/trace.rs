//! Host tests for the `trace` recorders (features `trace-hooks` + `macros`, see the
//! `[[test]]` entry in Cargo.toml).
//!
//! Declaring a real graph here exercises the full chain on the host: the macro's
//! generated statics AND the seven `_embassy_trace_*` hook symbols it emits under
//! `trace-hooks` (this test binary links them against embassy-executor's extern
//! declarations — a link failure would fail the test build). The nodes are
//! **parked** (no `spawn:`), so no executor is required: task ids are registered
//! manually via `set_task_id` (the documented parked-node path) and the recorders
//! are driven directly, with `embassy_time::MockDriver` supplying a controllable
//! clock.
//!
//! Everything shares global state (the registry, the executor slots, the mock
//! clock), so it is ONE sequential `#[test]` — the default parallel test harness
//! would race otherwise.

use embassy_supervisor::{supervisor_graph, trace};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node A = Terminate, deps: [];
    node B = Terminate, deps: [A];
}

/// Arbitrary executor id (a real one is the executor's address bits; any nonzero
/// value works for the recorders).
const EXEC: u32 = 7;

#[test]
fn trace_recorders() {
    let clock = MockDriver::get();
    trace::register_graph(&GRAPH.nodes[..]);

    // ── id -> node resolution + exec accounting ─────────────────────────
    A.set_task_id(101);
    B.set_task_id(202);
    assert_eq!(A.task_id(), 101);

    trace::on_task_exec_begin(EXEC, 101);
    clock.advance(Duration::from_ticks(100));
    trace::on_task_exec_end(EXEC, 101);
    assert_eq!(A.exec_ticks(), 100, "poll time attributed to A");
    assert_eq!(A.poll_count(), 1);
    assert_eq!(A.max_poll_ticks(), 100);
    assert_eq!(B.exec_ticks(), 0, "B untouched");

    // Second, shorter poll: ticks accumulate, watermark keeps the max.
    trace::on_task_exec_begin(EXEC, 101);
    clock.advance(Duration::from_ticks(40));
    trace::on_task_exec_end(EXEC, 101);
    assert_eq!(A.exec_ticks(), 140);
    assert_eq!(A.poll_count(), 2);
    assert_eq!(
        A.max_poll_ticks(),
        100,
        "watermark is the max, not the last"
    );

    // ── current_task / stalled_task during an open poll ─────────────────
    trace::on_task_exec_begin(EXEC, 202);
    clock.advance(Duration::from_ticks(30));
    let (node, running) = trace::current_task(EXEC).expect("a poll is in flight");
    assert_eq!(node.name, "b", "the macro lowercases node names");
    assert_eq!(running, 30);
    assert!(trace::stalled_task(EXEC, 10).is_some(), "30 ticks >= 10");
    assert!(trace::stalled_task(EXEC, 31).is_none(), "not yet past 31");
    trace::on_task_exec_end(EXEC, 202);
    assert!(trace::current_task(EXEC).is_none(), "nothing in flight");
    assert_eq!(B.exec_ticks(), 30);
    assert_eq!(B.max_poll_ticks(), 30);

    // ── idle accounting: the window opens at executor_idle, an EMPTY pass
    // (poll_start that polls nothing) does not interrupt it, and the next
    // exec_begin closes it ────────────────────────────────────────────────
    trace::on_executor_idle(EXEC);
    clock.advance(Duration::from_ticks(500));
    assert_eq!(
        trace::executor_idle_ticks(EXEC),
        500,
        "open idle window is included"
    );
    trace::on_poll_start(EXEC); // empty pass: no timestamp, window keeps running
    clock.advance(Duration::from_ticks(50));
    assert_eq!(
        trace::executor_idle_ticks(EXEC),
        550,
        "an empty pass merges into the idle window"
    );
    trace::on_executor_idle(EXEC); // idempotent while already idle
    clock.advance(Duration::from_ticks(25));
    assert_eq!(
        trace::executor_idle_ticks(EXEC),
        575,
        "re-idling does not restart the window"
    );

    // ── unknown ids: counted at the executor level, attributed to no node;
    // the exec_begin also closes (banks) the idle window ─────────────────
    let (a_before, b_before) = (A.exec_ticks(), B.exec_ticks());
    let st0 = trace::executor_stats(EXEC).expect("tracked executor");
    trace::on_task_exec_begin(EXEC, 999);
    clock.advance(Duration::from_ticks(60));
    trace::on_task_exec_end(EXEC, 999);
    assert_eq!((A.exec_ticks(), B.exec_ticks()), (a_before, b_before));
    let st1 = trace::executor_stats(EXEC).expect("tracked executor");
    assert_eq!(
        st1.exec_ticks.wrapping_sub(st0.exec_ticks),
        60,
        "unknown-id poll still counts as executor in-poll time"
    );
    assert_eq!(st1.polls.wrapping_sub(st0.polls), 1);
    assert_eq!(st1.passes, 1, "one poll_start so far");
    assert_eq!(
        st1.idle_ticks, 575,
        "window banked by exec_begin, then stable"
    );
    // The decomposition this enables: in-poll includes all polls (170 attributed
    // to A/B above + 60 unknown), so busy - exec_ticks isolates pure overhead.
    assert_eq!(st1.exec_ticks, 100 + 40 + 30 + 60);
    assert!(trace::executor_stats(31337).is_none(), "untracked id");

    // ── respawn overwrites the mapping ───────────────────────────────────
    A.set_task_id(303); // "respawned" with a fresh storage slot
    trace::on_task_exec_begin(EXEC, 101); // stale old id
    trace::on_task_exec_end(EXEC, 101);
    assert_eq!(A.poll_count(), 2, "old id no longer resolves to A");
    trace::on_task_exec_begin(EXEC, 303);
    clock.advance(Duration::from_ticks(5));
    trace::on_task_exec_end(EXEC, 303);
    assert_eq!(A.poll_count(), 3, "new id does");

    // ── task_end clears the mapping, but only if still current ──────────
    trace::on_task_end(EXEC, 303);
    assert_eq!(A.task_id(), 0, "cleared on task end");
    A.set_task_id(404);
    trace::on_task_end(EXEC, 303); // stale end (already respawned): no effect
    assert_eq!(A.task_id(), 404, "stale task_end must not clear a fresh id");

    // ── executor table ───────────────────────────────────────────────────
    assert!(trace::executors().contains(&EXEC));
    assert_eq!(trace::executor_idle_ticks(31337), 0, "untracked executor");
}
