//! `executor: NAME` must reference an `executor NAME;` declared in the same
//! graph; an unknown name is a macro-expansion error listing the declared slots.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    executor HIGH;
    node A = Terminate, deps: [], executor: TYPO, spawn: worker;
}

fn main() {}
