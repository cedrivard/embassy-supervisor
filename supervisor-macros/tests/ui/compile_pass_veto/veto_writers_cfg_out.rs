use embassy_supervisor::{TaskNode, supervisor_graph};

// The gate and its only writers ride one feature: a build without it must
// not name the gate, so the emitted capacity check is gated the same way.
#[cfg(feature = "nope")]
pub static TRIP: embassy_supervisor::VetoGate<2> = embassy_supervisor::VetoGate::new();

async fn protector(_node: &'static TaskNode) {}

supervisor_graph! {
    #[cfg(feature = "nope")]
    node OC = Terminate, deps: [], task: protector, writes: [crate::TRIP veto];
    node DIFF = Terminate, deps: [], task: protector,
        writes: [#[cfg(feature = "nope")] crate::TRIP veto];
    node KEEP = Terminate, deps: [], task: protector;
}

fn main() {
    assert!(DIFF.writes().iter().all(|t| t.is_empty()), "the gated write compiled out");
}
