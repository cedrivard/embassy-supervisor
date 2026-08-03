//! Same rule under `cancel`, where the shell holds a `Result<Output, Aborted>`:
//! a diverging worker makes the `Ok` arm's provide dead code, so `exit:` asks
//! for a value that can never exist. `cancel` itself stays legal on a diverging
//! worker — that is the shape it was added for; only the `exit:` clause is
//! rejected.

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
