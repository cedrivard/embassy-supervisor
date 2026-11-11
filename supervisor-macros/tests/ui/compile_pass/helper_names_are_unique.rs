// Items whose names would make two private helpers collide under a name-only
// scheme: `X`'s table set beside node `TBLS_X`, and pool `P`'s member 0 beside
// node `P_0`. Across graphs: item 1 `B_2_C` of graph `A` beside item 2 `C` of
// graph `A_1_B` (both would tag as `A_1_B_2_C`). Two items that differ only in
// case share a runtime name and are refused (`compile_fail/runtime_name_case.rs`).
use core::sync::atomic::AtomicU32;

use embassy_supervisor::{TaskNode, supervisor_graph};

pub static SIG: AtomicU32 = AtomicU32::new(0);

async fn worker(_node: &'static TaskNode) {}
async fn holder(_node: &'static TaskNode, _v: &mut u32) {}

supervisor_graph! {
    node X = Terminate, deps: [], task: worker, reads: [crate::SIG];
    node TBLS_X = Terminate, deps: [], task: worker, reads: [crate::SIG];
    pool P = [Terminate], deps: [], task: holder, resources: [PV: u32],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 1;
    node P_0 = Terminate, deps: [], task: holder, resources: [QV: u32];
    node NET = Terminate, deps: [], task: worker;
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 5);
    assert_eq!(A.nodes.len(), 2);
    assert_eq!(A_1_B.nodes.len(), 3);
}

supervisor_graph! {
    name: A;
    node A_ZERO = Terminate, deps: [], task: worker;
    node B_2_C = Terminate, deps: [], task: worker;
}

supervisor_graph! {
    name: A_1_B;
    node Z_ZERO = Terminate, deps: [], task: worker;
    node Z_ONE = Terminate, deps: [], task: worker;
    node C = Terminate, deps: [], task: worker;
}
