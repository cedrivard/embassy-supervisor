//! After `deps: [..]`, only `spawn:` or `disabled` may follow a node — anything else
//! is a compile error.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [], oops;
}

fn main() {}
