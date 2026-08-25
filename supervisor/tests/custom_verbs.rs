use std::sync::atomic::{AtomicU32, Ordering};

use embassy_supervisor::{Coupling, Sig, TaskNode, dataflow, supervisor_graph};

pub static ESTIMATE: AtomicU32 = AtomicU32::new(0);
pub static ARMED: AtomicU32 = AtomicU32::new(0);
pub static PLAIN: AtomicU32 = AtomicU32::new(0);

static SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

/// The consumer's own verbs. They take [`Sig`], which is what the rewrite
pub trait Signals {
    fn subscribe<T: Sync + ?Sized>(&'static self, s: Sig<T>) -> &'static T;
    fn publish(&'static self, s: Sig<AtomicU32>, v: u32);
    fn entry_of(&'static self, s: Sig<AtomicU32>) -> &'static Coupling;
    /// NOT a verb: an ordinary method taking the signal itself, which is what
    /// an unregistered access looks like.
    fn peek(&'static self, sig: &'static AtomicU32) -> u32;
}

impl Signals for TaskNode {
    fn subscribe<T: Sync + ?Sized>(&'static self, s: Sig<T>) -> &'static T {
        SEEN.lock().unwrap().push(s.entry.name());
        s.target
    }

    fn publish(&'static self, s: Sig<AtomicU32>, v: u32) {
        SEEN.lock().unwrap().push(s.entry.name());
        s.target.store(v, Ordering::Relaxed);
    }

    fn entry_of(&'static self, s: Sig<AtomicU32>) -> &'static Coupling {
        s.entry
    }

    fn peek(&'static self, sig: &'static AtomicU32) -> u32 {
        sig.load(Ordering::Relaxed)
    }
}

#[dataflow(read(subscribe), write(publish))]
async fn worker(node: &'static TaskNode) {
    node.subscribe(&crate::ESTIMATE);
    node.publish(&crate::ARMED, 1);
    // A built-in verb in the same body: registrations are additive.
    node.put(&crate::PLAIN, 2);
    // Neither built-in nor registered, and so not touched by the walk.
    node.set_ready();
}

/// The same access through a method that is not a verb here. It performs; the
/// coupling is simply not recorded.
#[dataflow]
async fn quiet(node: &'static TaskNode) {
    node.peek(&crate::ESTIMATE);
}

supervisor_graph! {
    node WORKER = Terminate, deps: [], task: worker, discover;
    node QUIET = Terminate, deps: [], task: quiet, discover;
}

#[dataflow(read(subscribe), write(publish))]
fn drive(node: &'static TaskNode) {
    node.subscribe(&crate::ESTIMATE);
    node.publish(&crate::ARMED, 7);
}

#[dataflow(write(publish), read(entry_of))]
fn armed_entry(node: &'static TaskNode) -> &'static Coupling {
    node.entry_of(&crate::ARMED)
}

fn names(tables: &[&[Coupling]]) -> Vec<&'static str> {
    tables
        .iter()
        .flat_map(|t| t.iter())
        .map(Coupling::name)
        .collect()
}

#[test]
fn a_registered_verb_records_like_a_built_in_one() {
    assert_eq!(
        names(WORKER.reads()),
        ["crate::ESTIMATE"],
        "`read(subscribe)` said read"
    );
    assert_eq!(
        names(WORKER.writes()),
        ["crate::ARMED", "crate::PLAIN"],
        "`write(publish)` said write, beside the built-in `put`"
    );

    assert!(
        names(QUIET.reads()).is_empty() && names(QUIET.writes()).is_empty(),
        "not a verb here, so no coupling: {:?}",
        names(QUIET.reads())
    );

    let estimate = &WORKER.reads()[0][0];
    let armed = &WORKER.writes()[0][0];
    let mut readers = Vec::new();
    GRAPH.readers_of(estimate, &mut |_, n| readers.push(n.name()));
    assert_eq!(
        readers,
        ["worker"],
        "a custom verb's read answers the signal-indexed query"
    );
    let mut writers = Vec::new();
    GRAPH.writers_of(armed, &mut |_, n| writers.push(n.name()));
    assert_eq!(writers, ["worker"], "and its write");
    let mut wrong_way = Vec::new();
    GRAPH.writers_of(estimate, &mut |_, n| wrong_way.push(n.name()));
    assert!(
        wrong_way.is_empty(),
        "direction is what the registration declared, not guesswork"
    );

    SEEN.lock().unwrap().clear();
    drive(&WORKER);
    assert_eq!(ARMED.load(Ordering::Relaxed), 7, "`publish` performed");
    assert_eq!(
        *SEEN.lock().unwrap(),
        ["crate::ESTIMATE", "crate::ARMED"],
        "each call site carried its own table entry"
    );

    assert_eq!(armed_entry(&WORKER).name(), "crate::ARMED");

    ESTIMATE.store(5, Ordering::Relaxed);
    assert_eq!(WORKER.peek(&ESTIMATE), 5);
}
