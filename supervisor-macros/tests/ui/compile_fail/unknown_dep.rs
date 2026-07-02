//! Depending on a name that isn't a declared node is a compile error.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [MISSING];
}

fn main() {}
