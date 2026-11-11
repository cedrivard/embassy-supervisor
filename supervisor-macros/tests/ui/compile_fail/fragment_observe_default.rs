
use embassy_supervisor::{TaskNode, supervisor_fragment};

pub async fn net_worker(_node: &'static TaskNode) {}

supervisor_fragment! {
    name: NET_FRAG;
    observe writes: it.get();
    node NET = Terminate, deps: [], task: crate::net_worker;
}

fn main() {}
