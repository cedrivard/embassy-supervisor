
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
