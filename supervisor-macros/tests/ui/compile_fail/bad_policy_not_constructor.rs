//! A pool `policy:` with no explicit type must be a `Type::new(..)`-shaped constructor
//! call so the macro can extract the policy TYPE (a path with >= 2 segments). A
//! single-segment call like `make_policy(dur)` has no type to strip, so `policy_type`
//! errors — the fix is the explicit `policy: <Type> = <expr>` form (see the error).

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    pool P = [Terminate], deps: [],
        spawn: worker,
        policy: make_policy(dur),
        min: 1, max: 1;
}

fn main() {}
