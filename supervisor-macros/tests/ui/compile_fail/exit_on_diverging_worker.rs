
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn diverging(_node: &'static TaskNode) -> ! {
    loop {
        core::future::pending::<()>().await;
    }
}

supervisor_graph! {
    node D = Terminate, deps: [], task: diverging, exit: u32;
}

fn main() {}
