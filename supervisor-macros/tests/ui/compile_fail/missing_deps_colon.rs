//! The `deps` keyword must be followed by `:`. Omitting it is a parse error.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps [];
}

fn main() {}
