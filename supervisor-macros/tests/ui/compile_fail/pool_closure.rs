//! A pool member can't be a closure (members are instantiated per index), so a pool
//! `spawn:` must be a task-fn path or a partial call — a closure is a compile error.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    pool P = [Terminate, OnDemand], deps: [],
        spawn: |_s| {},
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {}
