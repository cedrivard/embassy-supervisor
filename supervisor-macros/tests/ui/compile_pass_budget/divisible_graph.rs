use embassy_supervisor::{Claimant, DeferredShrink, TaskNode, supervisor_graph};

async fn allocator(_node: &'static TaskNode) {
    POWER.provide(100);
}

async fn holder(_node: &'static TaskNode, power: Claimant) {
    power.want(10);
}

async fn member(_node: &'static TaskNode, power: Claimant, _extra: u32) {
    power.want(5);
}

supervisor_graph! {
    node ALLOC = Terminate, deps: [], task: allocator, provides: [POWER];
    node ONE = Terminate, deps: [ALLOC], task: holder, resources: [POWER: divisible];
    node TWO = Terminate, deps: [ALLOC], task: holder,
        resources: [#[cfg(all())] POWER: divisible];
    pool MANY = [Terminate, OnDemand], deps: [ALLOC], task: member(1),
        resources: [POWER: divisible],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    // One slot per declaring node, one per pool member, in declaration order.
    let _: &embassy_supervisor::Budget<4> = &POWER;
    assert_eq!(POWER.slots(), 4);
}
