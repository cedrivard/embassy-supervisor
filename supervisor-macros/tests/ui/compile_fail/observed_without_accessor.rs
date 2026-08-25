
use embassy_supervisor::{TaskNode, supervisor_graph};

pub struct Silent;
pub static X: Silent = Silent;

async fn w(_n: &'static TaskNode) {}

supervisor_graph! {
    node A = Terminate, deps: [], task: w, writes: [crate::X observed];
}

fn main() {}
