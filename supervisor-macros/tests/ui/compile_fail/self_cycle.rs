//! A node depending on itself is a 1-node cycle: `topo_sort_const` never produces
//! it, so const-eval panics at build time. (cycle.rs covers the 2-node case.)

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [A];
}

fn main() {}
