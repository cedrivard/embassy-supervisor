// A gated `beat_timeout:` gates the whole liveness claim: `ready_on_write`
// rides the monitor sweep that budget arms, so it must carry the identical
// `#[cfg]` predicate.
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

fn accessor() -> u32 {
    0
}

pub static S: u32 = 0;

supervisor_graph! {
    node N = Terminate, deps: [], task: worker,
        writes: [crate::S observed beat via accessor()],
        #[cfg(feature = "x")] beat_timeout: 100,
        ready_on_write;
}

fn main() {}
