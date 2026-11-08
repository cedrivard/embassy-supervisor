// A marked dep on a compiled-out node drops out of the marker arrays with it.
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node NET = Terminate, deps: [], task: worker;
    #[cfg(any())]
    node GONE = Terminate, deps: [], task: worker;
    node HTTP = Terminate, deps: [NET ready, GONE ready bound], task: worker;
}

fn main() {
    assert_eq!(HTTP.ready_deps().len(), 1);
    assert!(core::ptr::eq(HTTP.ready_deps()[0], &NET));
    assert!(HTTP.bound_deps().is_empty());
}
