#![allow(unused_macros)]
// Local macros named like std's: the expansion must reach `::core`'s.

use core::sync::atomic::AtomicU32;

use embassy_supervisor::{TaskNode, dataflow, supervisor_graph};

macro_rules! assert {
    ($($t:tt)*) => {
        compile_error!("the expansion used the local `assert!`")
    };
}
macro_rules! concat {
    ($($t:tt)*) => {
        compile_error!("the expansion used the local `concat!`")
    };
}
macro_rules! stringify {
    ($($t:tt)*) => {
        compile_error!("the expansion used the local `stringify!`")
    };
}

pub static OUT: AtomicU32 = AtomicU32::new(0);

#[dataflow]
async fn w(node: &'static TaskNode) {
    node.put(&crate::OUT, 1);
}

async fn member(_node: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: w, discover, writes: [crate::OUT observed beat];
    pool P = [Terminate, OnDemand], deps: [], task: member,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    ::core::assert_eq!(GRAPH.nodes.len(), 3);
}
