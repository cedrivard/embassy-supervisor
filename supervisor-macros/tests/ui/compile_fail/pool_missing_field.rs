//! Pool fields are mandatory and strictly ordered. Omitting `policy:` (jumping from
//! `spawn:` straight to `min:`) is a parse error at the expected `policy` keyword.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    pool P = [Terminate], deps: [A],
        spawn: worker,
        min: 1, max: 1;
}

fn main() {}
