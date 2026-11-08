// A local `drop` must not capture the `local` slot type's `clear()`.
use embassy_supervisor::{TaskNode, supervisor_graph};

fn drop(_: ()) {}

async fn worker(_node: &'static TaskNode, _v: &mut u32) {}

supervisor_graph! {
    node N = Terminate, deps: [], task: worker, resources: [V: local u32];
}

fn main() {
    drop(());
    V.provide(7);
    ::core::assert_eq!(V.take(), Some(7));
}
