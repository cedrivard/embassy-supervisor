//! A pool's `executor: NAME` must reference a declared `executor NAME;`, same
//! as a node's — an unknown name is a macro-expansion error.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    executor HIGH;
    pool P = [Terminate], deps: [], executor: TYPO,
        spawn: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 1;
}

fn main() {}
