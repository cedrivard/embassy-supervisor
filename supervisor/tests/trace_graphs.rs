use embassy_supervisor::{graphs, supervisor_graph, trace};
use embassy_time::{Duration, MockDriver};

supervisor_graph! {
    node N0 = Terminate, deps: [];
}
supervisor_graph! {
    name: G1;
    node N1 = Terminate, deps: [];
}
supervisor_graph! {
    name: G2;
    node N2 = Terminate, deps: [];
}
supervisor_graph! {
    name: G3;
    node N3 = Terminate, deps: [];
}
supervisor_graph! {
    name: G4;
    node N4 = Terminate, deps: [];
}
supervisor_graph! {
    name: G5;
    node N5 = Terminate, deps: [];
}

const EXEC: u32 = 11;

fn poll(clock: &MockDriver, task_id: u32) {
    trace::on_task_exec_begin(EXEC, task_id);
    clock.advance(Duration::from_ticks(10));
    trace::on_task_exec_end(EXEC, task_id);
}

#[test]
fn every_registered_graph_resolves() {
    let clock = MockDriver::get();
    let nodes = [&N0, &N1, &N2, &N3, &N4, &N5];
    for (i, n) in nodes.iter().enumerate() {
        n.set_task_id(100 + i as u32);
    }

    poll(clock, 100);
    assert_eq!(N0.poll_count(), 0, "an unlinked graph is invisible");

    let refs = [
        GRAPH.graph_ref,
        G1.graph_ref,
        G2.graph_ref,
        G3.graph_ref,
        G4.graph_ref,
        G5.graph_ref,
    ];
    for g in refs {
        trace::register_graph(g);
    }
    for g in refs {
        trace::register_graph(g);
    }
    assert_eq!(
        graphs().count(),
        6,
        "every graph linked exactly once, and none dropped"
    );

    for (i, n) in nodes.iter().enumerate() {
        poll(clock, 100 + i as u32);
        assert_eq!(
            n.poll_count(),
            1,
            "{} did not resolve: graph {i} is not reachable",
            n.name()
        );
        assert_eq!(n.exec_ticks(), 10);
    }

    assert!(
        nodes.iter().all(|n| n.poll_count() == 1),
        "one poll each, no cross-graph double counting"
    );

    let before = trace::executor_stats(EXEC).expect("tracked executor");
    poll(clock, 9999);
    let after = trace::executor_stats(EXEC).expect("tracked executor");
    assert_eq!(after.polls.wrapping_sub(before.polls), 1);
    assert!(
        nodes.iter().all(|n| n.poll_count() == 1),
        "attributed to no node"
    );
}
