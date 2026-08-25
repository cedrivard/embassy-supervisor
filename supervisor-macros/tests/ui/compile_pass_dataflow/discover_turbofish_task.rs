
use embassy_supervisor::{TaskNode, dataflow, supervisor_graph};

pub static OUT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[dataflow]
async fn worker<T: Into<u32> + Default>(node: &'static TaskNode) {
    node.put(&crate::OUT, T::default().into());
    node.ack_dropped();
}

supervisor_graph! {
    node N = Terminate, deps: [], task: worker::<u8>, discover;
}

fn main() {
    assert_eq!(N.writes().len(), 1);
    assert_eq!(N.writes()[0][0].name(), "crate::OUT");
}
