//! A `#[cfg(...)]` on a dependency does NOT rescue an unknown name: the macro
//! resolves dep names to indices at expansion time (before cfg is evaluated), so a
//! cfg-gated reference to an undeclared node is still a compile error.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [#[cfg(feature = "x")] MISSING];
}

fn main() {}
