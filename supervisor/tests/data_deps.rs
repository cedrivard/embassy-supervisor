use core::sync::atomic::{AtomicU32, Ordering};

use embassy_supervisor::{
    Coupling, DeferredShrink, TaskNode, dataflow, producer_of, supervisor_graph,
};
use embassy_time::{Duration, MockDriver};

pub static ALPHA_SIG: AtomicU32 = AtomicU32::new(0);
pub static BETA_SIG: AtomicU32 = AtomicU32::new(0);
pub static NOBODY_WRITES: AtomicU32 = AtomicU32::new(0);

#[dataflow]
async fn alpha_producer(node: &'static TaskNode) {
    node.beat_writer(&crate::ALPHA_SIG)
        .store(1, Ordering::Relaxed);
}

#[dataflow]
async fn alpha_consumer(node: &'static TaskNode) {
    node.reader(&crate::ALPHA_SIG);
    node.reader(&crate::NOBODY_WRITES);
}

#[dataflow]
async fn beta_worker(node: &'static TaskNode) {
    node.reader(&crate::ALPHA_SIG);
    node.put(&crate::BETA_SIG, 1);
}

async fn pool_worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node ALPHA_PROD = Terminate, deps: [], task: alpha_producer, discover;
    node ALPHA_CONS = Terminate, deps: [], task: alpha_consumer, discover;
    pool ALPHA_POOL = [Terminate, OnDemand], deps: [], task: pool_worker,
        writes: [crate::ALPHA_SIG],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3600)),
        min: 1, max: 2;
}

supervisor_graph! {
    name: BETA_GRAPH;
    node BETA = Terminate, deps: [], task: beta_worker, discover;
}

fn alpha_entry() -> &'static Coupling {
    &ALPHA_CONS.reads()[0][0]
}

/// Drives the heartbeat write from the test, through the caller's node.
#[dataflow]
fn pulse(node: &'static TaskNode) {
    node.beat_writer(&crate::ALPHA_SIG)
        .store(2, Ordering::Relaxed);
}

/// A plain pass-through write to the same signal: a coupling, not a claim.
#[dataflow]
fn quiet(node: &'static TaskNode) {
    node.writer(&crate::ALPHA_SIG).store(3, Ordering::Relaxed);
}

#[test]
fn a_node_answers_for_its_own_graph() {
    assert!(!ALPHA_PROD.is_running());

    let alpha: Vec<&str> = ALPHA_PROD
        .graph()
        .iter()
        .flatten()
        .map(|n| n.name())
        .collect();
    assert_eq!(
        alpha,
        ["alpha-prod", "alpha-cons", "alpha-pool0", "alpha-pool1"]
    );
    assert_eq!(
        BETA.graph()
            .iter()
            .flatten()
            .map(|n| n.name())
            .collect::<Vec<_>>(),
        ["beta"]
    );

    assert!(core::ptr::eq(ALPHA_POOL[0].graph(), ALPHA_PROD.graph()));
    assert!(core::ptr::eq(ALPHA_POOL[1].graph(), ALPHA_PROD.graph()));

    let prod = producer_of(&ALPHA_CONS, alpha_entry()).expect("alpha has a writer");
    assert_eq!(prod.name(), "alpha-prod", "the first writer in topo order");

    assert!(
        producer_of(&BETA, alpha_entry()).is_none(),
        "a node resolves within its own graph, never across"
    );

    assert!(producer_of(&ALPHA_CONS, &ALPHA_CONS.reads()[0][1]).is_none());
}

#[test]
fn beat_writer_is_the_claim_and_writer_is_not() {
    let clock = MockDriver::get();
    clock.advance(Duration::from_millis(50));
    assert!(
        ALPHA_CONS.ticks_since_beat() > 0,
        "a node that never beat is not fresh"
    );

    quiet(&ALPHA_CONS);
    assert_eq!(
        ALPHA_SIG.load(Ordering::Relaxed),
        3,
        "`writer` handed it back"
    );
    assert!(
        ALPHA_CONS.ticks_since_beat() > 0,
        "a plain `writer` says nothing about the node"
    );

    pulse(&ALPHA_CONS);
    assert_eq!(ALPHA_SIG.load(Ordering::Relaxed), 2, "`beat_writer` too");
    assert_eq!(
        ALPHA_CONS.ticks_since_beat(),
        0,
        "`beat_writer` flagged a beat, and this check granted it"
    );
}
