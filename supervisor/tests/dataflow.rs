use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, dataflow, supervisor_graph};
use embassy_time::{Duration, MockDriver};

pub static IN: AtomicU32 = AtomicU32::new(0);
pub static POOL_OUT: AtomicU32 = AtomicU32::new(0);
pub static OUT: AtomicU32 = AtomicU32::new(0);
pub static SCRATCH: AtomicU32 = AtomicU32::new(0);
pub static ROGUE: AtomicU32 = AtomicU32::new(0);

#[dataflow]
async fn scout_task(node: &'static TaskNode) {
    node.get(&crate::OUT);
    node.put(&crate::SCRATCH, 0);
}

#[dataflow]
async fn eaves_task(node: &'static TaskNode) {
    node.reader(&crate::OUT);
}

#[dataflow]
fn put_out(node: &'static TaskNode, v: u32) {
    node.put(&crate::OUT, v);
}

/// The same write, carrying the node's heartbeat. The verb is the declaration:
#[dataflow]
fn beat_out(node: &'static TaskNode, v: u32) {
    node.beat_put(&crate::OUT, v);
}

#[dataflow]
fn get_in(node: &'static TaskNode) -> u32 {
    node.get(&crate::IN)
}

#[dataflow]
fn bump_out(node: &'static TaskNode) {
    node.writer(&crate::OUT).fetch_add(1, Ordering::Relaxed);
}

#[dataflow]
fn read_in_ref(node: &'static TaskNode) -> &'static AtomicU32 {
    node.reader(&crate::IN)
}

#[dataflow]
fn touch_rogue(node: &'static TaskNode) {
    node.get(&crate::ROGUE);
}

#[dataflow]
fn touch_scratch(node: &'static TaskNode) {
    node.get(&crate::SCRATCH);
}

/// An accessor: callers pass their node; adopters bind this fn's table, so
#[dataflow]
fn set_scratch(node: &'static TaskNode, v: u32) {
    node.put(&crate::SCRATCH, v);
}

#[dataflow]
fn push_pool_out(node: &'static TaskNode, v: u32) {
    node.put(&crate::POOL_OUT, v);
}

async fn pool_worker(_node: &'static TaskNode) {}

supervisor_graph! {
    // Declared entries rather than derived ones, and no marker among them:
    // the heartbeat is the verb's business now, and readiness is the body's.
    node WORKER = Terminate, deps: [],
        reads: [crate::IN],
        writes: [crate::OUT];

    // The derived tier: no lists; the task fns' tables bind instead.
    node SCOUT = Terminate, deps: [WORKER], task: scout_task, discover;

    node EAVES = Terminate, deps: [], task: eaves_task, discover;

    node BYSTANDER = Terminate, deps: [], reads: [crate::ROGUE];

    node ADOPTER = Terminate, deps: [SCOUT],
        writes: [crate::SCRATCH],
        dataflow: [crate::set_scratch];

    pool WORKERS = [Terminate, OnDemand], deps: [], task: pool_worker,
        writes: [crate::POOL_OUT],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(3)),
        min: 1, max: 2;
}

static SUP: Supervisor<7, GRAPH_TOPOLOGY> = Supervisor::new(&GRAPH);
static DONE: AtomicBool = AtomicBool::new(false);

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let clock = MockDriver::get();

    assert_eq!(SCOUT.reads().len(), 1);
    assert!(
        SCOUT.writes()[0][0].name() == "crate::SCRATCH",
        "a derived entry states the coupling, and states nothing else"
    );

    let mut readers = Vec::new();
    GRAPH.readers_of(&SCOUT.writes()[0][0], &mut |_, n| readers.push(n.name()));
    let mut consumers = Vec::new();
    GRAPH.readers_of(&EAVES.reads()[0][0], &mut |_, n| consumers.push(n.name()));
    assert!(
        consumers.contains(&"eaves") && consumers.contains(&"scout"),
        "derived read tables answer `readers_of`: {consumers:?}"
    );
    assert!(
        readers.is_empty(),
        "SCRATCH is written and never read: `readers_of` answers empty \
         rather than echoing the writers: {readers:?}"
    );

    clock.advance(Duration::from_millis(500));
    put_out(&WORKER, 7);
    assert_eq!(OUT.load(Ordering::Relaxed), 7, "`put` performed the write");
    assert!(
        WORKER.ticks_since_beat() > 0,
        "a plain `put` is not a heartbeat: it says nothing about the node"
    );
    clock.advance(Duration::from_millis(500));
    beat_out(&WORKER, 7);
    assert_eq!(
        WORKER.ticks_since_beat(),
        0,
        "`beat_put` flagged a beat, and this very check granted it"
    );
    assert_eq!(get_in(&WORKER), 0, "`get` performed the read");
    bump_out(&WORKER);
    assert_eq!(
        OUT.load(Ordering::Relaxed),
        8,
        "`writer` handed the RMW back"
    );
    assert!(std::ptr::eq(read_in_ref(&WORKER), &IN));

    assert_eq!(ADOPTER.writes().len(), 2, "declared list + adopted table");
    assert_eq!(ADOPTER.writes()[1][0].name(), "crate::SCRATCH");
    set_scratch(&ADOPTER, 5);
    assert_eq!(SCRATCH.load(Ordering::Relaxed), 5);

    push_pool_out(&WORKERS[1], 4);
    assert_eq!(POOL_OUT.load(Ordering::Relaxed), 4);

    touch_rogue(&BYSTANDER);
    SCRATCH.store(9, Ordering::Relaxed);
    touch_scratch(&BYSTANDER);

    WORKER.report_status("steady");
    assert_eq!(WORKER.status(), Some("steady"));
    SUP.start(&spawner).await.expect("start");
    assert_eq!(WORKER.status(), None);

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn the_body_is_the_declaration() {
    let _clock = MockDriver::get();

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
