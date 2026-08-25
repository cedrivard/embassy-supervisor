
use embassy_supervisor::supervisor_graph;

async fn diverging() -> ! {
    loop {
        core::future::pending::<()>().await;
    }
}

supervisor_graph! {
    node D = Terminate, deps: [], task: diverging, cancel, exit: u32;
}

fn main() {}
