use embassy_supervisor::{DeferredShrink, PriorityError, TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

#[embassy_executor::task]
async fn hand(_node: &'static TaskNode) {}

supervisor_graph! {
    executor HIGH;
    supervisor priority: 7;
    node A = Terminate, deps: [], priority: 5, task: worker;
    node B = Terminate, deps: [], executor: HIGH, priority: 7, task: worker;
    node C = Terminate, deps: [], task: worker;
    node D = Terminate, deps: [], #[cfg(all())] priority: 3, task: worker;
    node E = Terminate, deps: [], #[cfg(any())] priority: 3, task: worker;
    node F = Terminate, deps: [], spawn: hand;
    pool P = [Terminate, OnDemand], deps: [], priority: 9, task: worker,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2;
}

fn main() {
    assert_eq!(GRAPH.supervisor_priority, 7);
    assert_eq!(A.priority(), Some(5));
    assert_eq!(B.priority(), Some(7));
    assert_eq!(C.priority(), Some(0));
    assert_eq!(D.priority(), Some(3));
    assert_eq!(E.priority(), Some(0));
    assert_eq!(F.priority(), None);
    assert_eq!(F.set_priority(1), Err(PriorityError::NoShell));
    assert_eq!(P[1].priority(), Some(9));
    assert_eq!(C.set_priority(6), Ok(()));
    assert_eq!(C.priority(), Some(6));
}
