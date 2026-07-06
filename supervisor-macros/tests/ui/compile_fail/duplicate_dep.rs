//! A repeated dependency is a compile error: a doubled index would be counted
//! twice in the in-degree but decremented once by `topo_sort_const`, surfacing
//! as a bogus "dependency cycle" instead of pointing at the real mistake.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    node B = Terminate, deps: [A, A];
}

fn main() {}
