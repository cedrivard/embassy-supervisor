//! `exit:` on a worker that can never return: its future has no output, so the
//! slot could never be filled and every `wait_take()` on it would hang. The
//! generated provide re-denies `unreachable_code` on itself, spanned on the
//! `exit:` clause the user has to drop.

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
