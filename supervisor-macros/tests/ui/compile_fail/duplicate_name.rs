//! A redeclared node/pool name is a compile error at the macro level (not just
//! the downstream `duplicate definition of static`): deps resolve through the
//! name map, so a silent overwrite would silently rewire earlier `deps:` edges.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    node A = Pause, deps: [];
}

fn main() {}
