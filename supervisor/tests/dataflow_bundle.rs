use std::sync::atomic::AtomicU32;

use embassy_supervisor::{TaskNode, supervisor_graph};

pub static A: AtomicU32 = AtomicU32::new(0);
pub static B: AtomicU32 = AtomicU32::new(0);
pub static C: AtomicU32 = AtomicU32::new(0);
pub static D: AtomicU32 = AtomicU32::new(0);

#[embassy_supervisor::dataflow_bundle]
pub mod api {
    use super::*;
    use embassy_supervisor::dataflow;

    #[dataflow]
    pub fn set_a(node: &'static TaskNode, v: u32) {
        node.put(&crate::A, v);
    }

    #[dataflow]
    pub fn read_b(node: &'static TaskNode) -> u32 {
        node.get(&crate::B)
    }

    #[cfg(any())]
    #[dataflow]
    pub fn gated(node: &'static TaskNode) {
        node.put(&crate::C, 1);
    }

    /// No attribute, no entry — the bundle walks `#[dataflow]` fns only.
    pub fn plain(_v: u32) {}
}

/// The named form, for a module carrying more than one bundle-adjacent thing.
#[embassy_supervisor::dataflow_bundle(EXTRA)]
pub mod more {
    use super::*;
    use embassy_supervisor::dataflow;

    #[dataflow]
    pub fn set_d(node: &'static TaskNode, v: u32) {
        node.put(&crate::D, v);
    }
}

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node USER = Terminate, task: worker,
        dataflow: [crate::api::BUNDLE, crate::more::EXTRA];
    node PROBE = Terminate, reads: [crate::A];
}

#[test]
fn a_bundle_is_the_members_tables_under_one_name() {
    // The statics themselves: A's write and B's read survive, the gated
    // member's entry compiled out with it, the plain fn contributed nothing.
    assert_eq!(api::__SV_DATAFLOW_WRITES_BUNDLE.len(), 1);
    assert_eq!(api::__SV_DATAFLOW_WRITES_BUNDLE[0].name(), "crate::A");
    assert_eq!(api::__SV_DATAFLOW_READS_BUNDLE.len(), 1);
    assert_eq!(api::__SV_DATAFLOW_READS_BUNDLE[0].name(), "crate::B");
    assert_eq!(more::__SV_DATAFLOW_WRITES_EXTRA.len(), 1);

    assert_eq!(USER.writes().len(), 2, "one table per adoption");
    let mut writers = Vec::new();
    GRAPH.writers_of(&PROBE.reads()[0][0], &mut |_, n| writers.push(n.name()));
    assert!(
        writers.contains(&"user"),
        "the bundle's write of A attributes to its adopter: {writers:?}"
    );
}
