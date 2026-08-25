use embassy_supervisor::{supervisor_graph, trace};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node LOW = Terminate, deps: [];
    node HIGHER = Terminate, deps: [];
}

const THREAD: u32 = 0x1000;
const INT: u32 = 0x2000;

#[test]
fn charge_splitting() {
    let clock = MockDriver::get();
    trace::register_graph(GRAPH.graph_ref);
    LOW.set_task_id(11);
    HIGHER.set_task_id(22);

    trace::on_task_exec_begin(THREAD, 11);
    clock.advance(Duration::from_ticks(30));
    trace::on_task_exec_begin(INT, 22);
    clock.advance(Duration::from_ticks(40));
    trace::on_task_exec_end(INT, 22);
    clock.advance(Duration::from_ticks(70));
    trace::on_task_exec_end(THREAD, 11);

    assert_eq!(HIGHER.exec_ticks(), 40, "tier's own time exact");
    assert_eq!(HIGHER.max_poll_ticks(), 40);
    assert_eq!(LOW.exec_ticks(), 100, "victim relieved of the stolen 40");
    assert_eq!(LOW.max_poll_ticks(), 100, "watermark not inflated to 140");
    let th = trace::executor_stats(THREAD).unwrap();
    let it = trace::executor_stats(INT).unwrap();
    assert_eq!(th.exec_ticks, 100, "executor-level in-poll corrected too");
    assert_eq!(it.exec_ticks, 40);

    const TOP: u32 = 0x3000;
    trace::on_task_exec_begin(THREAD, 11);
    clock.advance(Duration::from_ticks(10));
    trace::on_task_exec_begin(INT, 22);
    clock.advance(Duration::from_ticks(5));
    trace::on_task_exec_begin(TOP, 999);
    clock.advance(Duration::from_ticks(20));
    trace::on_task_exec_end(TOP, 999);
    clock.advance(Duration::from_ticks(5));
    trace::on_task_exec_end(INT, 22);
    clock.advance(Duration::from_ticks(10));
    trace::on_task_exec_end(THREAD, 11);

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
