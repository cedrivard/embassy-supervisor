use embassy_supervisor::{TaskNode, supervisor_fragment};

pub async fn net_worker(_node: &'static TaskNode) {}

supervisor_fragment! {
    name: NET_FRAG;
    supervisor priority: 3;
    node NET = Terminate, deps: [], task: crate::net_worker;
}

fn main() {}
