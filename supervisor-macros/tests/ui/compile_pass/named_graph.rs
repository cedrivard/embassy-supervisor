//! `name: X;` renames the emitted graph static and suffixes the private
//! backing tables, so two graphs coexist in ONE module (each with its own
//! name map, order, and 256 cap).

use embassy_supervisor::{TaskNode, supervisor_graph};

async fn a_worker(_node: &'static TaskNode) {}
async fn b_worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node MAIN = Terminate, deps: [], task: a_worker;
}

supervisor_graph! {
    name: SUB_GRAPH;
    node SENSOR = Terminate, deps: [], task: b_worker;
    node REPORT = Terminate, deps: [SENSOR], task: b_worker;
}

fn main() {
    assert_eq!(GRAPH.order.len(), 1);
    assert_eq!(SUB_GRAPH.order, [0, 1]);
    assert_eq!(MAIN.name, "main");
    assert_eq!(SENSOR.name, "sensor");
}
