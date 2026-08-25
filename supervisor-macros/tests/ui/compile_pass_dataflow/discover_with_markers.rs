
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_supervisor::{TaskNode, dataflow, supervisor_graph};

pub static IN: AtomicU32 = AtomicU32::new(0);
pub static EST: AtomicU32 = AtomicU32::new(0);

#[dataflow]
async fn w(node: &'static TaskNode) {
    node.get(&crate::IN);
}

/// An accessor the node adopts; the marked write lands here.
#[dataflow]
fn step(node: &'static TaskNode) {
    let v = node.get(&crate::IN);
    node.put(&crate::EST, v + 1);
}

supervisor_graph! {
    node A = Terminate, deps: [], task: w,
        discover, dataflow: [crate::step],
        writes: [crate::EST observed beat];
}

fn main() {
    assert_eq!(A.writes().len(), 3);
    let marked: Vec<&str> = A
        .writes()
        .iter()
        .flat_map(|t| t.iter())
        .filter(|c| c.beats())
        .map(|c| c.name())
        .collect();
    assert_eq!(marked, ["crate::EST"], "only the list entry is marked");

    assert!(
        A.writes()
            .iter()
            .flat_map(|t| t.iter())
            .any(|c| c.beats() && c.observer().is_some())
    );
    step(&A);
    assert_eq!(EST.load(Ordering::Relaxed), 1, "`put` performed the write");
}
