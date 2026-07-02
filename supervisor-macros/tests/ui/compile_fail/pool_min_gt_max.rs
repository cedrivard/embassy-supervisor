//! A pool's scaling bounds must satisfy `min <= max <= member count`; `min > max`
//! makes the policy contradict itself (always above the floor and below the
//! ceiling at once), so the macro rejects it at expansion time.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    pool P = [Terminate, OnDemand, OnDemand], deps: [],
        spawn: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 3, max: 2;
}

fn main() {}
