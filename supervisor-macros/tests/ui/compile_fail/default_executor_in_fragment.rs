
use embassy_supervisor::{TaskNode, supervisor_fragment};

pub async fn net_worker(_node: &'static TaskNode) {}

supervisor_fragment! {
    name: NET_FRAG;
    default executor THREAD;
    node NET = Terminate, deps: [], task: crate::net_worker;
}

fn main() {}
