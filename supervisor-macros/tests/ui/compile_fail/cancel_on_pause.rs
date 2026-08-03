//! `cancel` + `Mode::Pause` is contradictory: a Pause worker must survive the
//! stop and park on `wait_resume()`, but `cancel` drops its future and records
//! an exit — nothing would ever resume it.

use embassy_supervisor::supervisor_graph;

async fn worker() -> ! {
    loop {
        embassy_futures::yield_now().await;
    }
}

supervisor_graph! {
    node PROBE = Pause, deps: [], task: worker, cancel;
}

fn main() {}
