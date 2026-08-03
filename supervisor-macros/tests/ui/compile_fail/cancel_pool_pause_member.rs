//! `cancel` + a `Pause` member is the pool form of the node contradiction: the
//! parked member must survive its stop and resume, but `cancel` drops its future
//! and records an exit.

use embassy_supervisor::supervisor_graph;

async fn worker() -> ! {
    loop {
        embassy_futures::yield_now().await;
    }
}

supervisor_graph! {
    pool WORKERS = [Terminate, Pause], deps: [],
        task: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2, cancel;
}

fn main() {}
