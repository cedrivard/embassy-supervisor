
use core::sync::atomic::{AtomicU32, Ordering};
use embassy_supervisor::{TaskNode, dataflow, supervisor_graph};

pub static IN: AtomicU32 = AtomicU32::new(0);
pub static OUT: AtomicU32 = AtomicU32::new(0);
pub static FOUND: AtomicU32 = AtomicU32::new(0);

async fn f(_n: &'static TaskNode) {}

#[dataflow]
async fn g(node: &'static TaskNode) {
    let v = node.get(&crate::IN);
    node.put(&crate::FOUND, v);
    node.writer(&crate::OUT).fetch_add(1, Ordering::Relaxed);
}

#[dataflow]
fn drive(node: &'static TaskNode) {
    let v = node.get(&crate::IN);
    node.put(&crate::FOUND, v + 6);
    node.writer(&crate::OUT).store(9, Ordering::Relaxed);
}

/// An accessor: callers pass their node, adopters bind this fn's table, and
#[dataflow]
fn set_found(node: &'static TaskNode, v: u32) {
    node.put(&crate::FOUND, v);
}

/// A heartbeat write. The verb carries the liveness claim; the table it lands
/// in carries the coupling and says nothing about liveness at all.
#[dataflow]
async fn beats(node: &'static TaskNode) {
    node.beat_put(&crate::FOUND, 1);
}

supervisor_graph! {
    node A = Terminate, deps: [], task: f,
        reads: [crate::IN],
        writes: [crate::OUT observed beat];
    node B = Terminate, deps: [], task: g, discover;
    node C = Terminate, deps: [], task: f,
        reads: [crate::IN],
        dataflow: [crate::set_found];
    node D = Terminate, deps: [], task: beats, discover;
}

fn main() {
    let read = &A.reads()[0][0];
    assert!(!read.beats());
    let write = &A.writes()[0][0];
    assert!(write.beats() && write.observer().is_some());

    assert_eq!(B.reads()[0].len(), 1);
    assert_eq!(B.writes()[0].len(), 2);
    assert!(B.writes()[0].iter().all(|w| !w.beats()));

    assert_eq!(C.reads().len(), 2);
    assert_eq!(C.reads()[1].len(), 0);
    assert_eq!(C.writes().len(), 1);
    assert_eq!(C.writes()[0][0].name(), "crate::FOUND");

    let mut readers = Vec::new();
    GRAPH.readers_of(&B.reads()[0][0], &mut |_, n| readers.push(n.name()));
    assert_eq!(readers, ["a", "b", "c"], "{readers:?}");

    drive(&B);
    assert_eq!(FOUND.load(Ordering::Relaxed), 6, "`put` performed the write");
    set_found(&C, 9);
    assert_eq!(FOUND.load(Ordering::Relaxed), 9, "the accessor performed it");

    assert_eq!(D.writes()[0][0].name(), "crate::FOUND");
}
