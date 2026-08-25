
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_supervisor::{DeferredShrink, TaskNode, dataflow, supervisor_graph};

pub static ARR: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
pub static POOLED: AtomicU32 = AtomicU32::new(0);
pub static NEVER: AtomicU32 = AtomicU32::new(0);

#[dataflow]
async fn indexed(node: &'static TaskNode) {
    node.put(&crate::ARR[1], 1);
}

#[dataflow]
async fn pooled(node: &'static TaskNode) {
    node.put(&crate::POOLED, 1);
}

supervisor_graph! {
    node A = Terminate, deps: [], task: indexed, discover,
        writes: [
            crate::ARR[1] observed beat,
            #[cfg(feature = "nope")]
            crate::NEVER observed beat,
        ];

    pool P = [Terminate, OnDemand], deps: [], task: pooled, discover,
        writes: [crate::POOLED observed beat],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
}

fn main() {
    let marked: Vec<&str> = A
        .writes()
        .iter()
        .flat_map(|t| t.iter())
        .filter(|c| c.beats())
        .map(|c| c.name())
        .collect();
    assert_eq!(marked, ["crate::ARR[1]"]);

    indexed_write(&A);
    assert_eq!(ARR[1].load(Ordering::Relaxed), 1);
    assert_eq!(ARR[0].load(Ordering::Relaxed), 0);

    assert!(P[0].writes().iter().flat_map(|t| t.iter()).any(|c| c.beats()));
}

#[dataflow]
fn indexed_write(node: &'static TaskNode) {
    node.put(&crate::ARR[1], 1);
}
