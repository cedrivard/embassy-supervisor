use embassy_supervisor::{DeferredShrink, TaskNode, VetoGate, supervisor_graph};

pub static TRIP: VetoGate<4> = VetoGate::new();

async fn protector(_node: &'static TaskNode) {}
async fn breaker(_node: &'static TaskNode) {}

supervisor_graph! {
    node OC = Terminate, deps: [], task: protector, writes: [crate::TRIP veto];
    pool BF = [Terminate, OnDemand], deps: [], task: protector, writes: [crate::TRIP veto],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)), min: 1, max: 2;
    node DIFF = Terminate, deps: [], task: protector, writes: [crate::TRIP veto observed beat],
        beat_timeout: 100;
    node BREAKER = Terminate, deps: [], task: breaker, reads: [crate::TRIP];
}

fn main() {
    // Four writers for a gate of four: OC, BF x 2, DIFF.
    let slots: Vec<Option<u8>> = [&OC, &BF[0], &BF[1], &DIFF]
        .iter()
        .map(|n| n.writes().iter().flat_map(|t| t.iter()).find_map(|c| c.veto_slot()))
        .collect();
    assert_eq!(slots, [Some(0), Some(1), Some(2), Some(3)]);
}
