//! A take that comes up empty between the gate probe and the activation hands
//! back what the activation already took, lend and consume alike, before the
//! exit record.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, TaskNode, supervisor_graph};
use embassy_time::MockDriver;

struct Probe(u32);

static RAN: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

async fn lend_worker(_node: &'static TaskNode, _a: &mut Probe, _b: &mut Probe) {
    RAN.store(true, Ordering::SeqCst);
}

async fn consume_worker(_node: &'static TaskNode, _a: Probe, _b: &mut Probe) {
    RAN.store(true, Ordering::SeqCst);
}

supervisor_graph! {
    node LEND = Terminate, deps: [], task: lend_worker,
        resources: [LEND_A: Probe, LEND_B: Probe];
    node CONSUME = Terminate, deps: [], task: consume_worker,
        resources: [CONSUME_A: consume Probe, CONSUME_B: Probe];
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
    assert!(f(), "did not settle");
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    for (node, first, second) in [
        (&LEND, &LEND_A, &LEND_B),
        (&CONSUME, &CONSUME_A, &CONSUME_B),
    ] {
        first.provide(Probe(1));
        second.provide(Probe(2));
        // Nothing is awaited after the glue's spawn, so `start_node` returns
        // before the shell's first poll and the take below wins the race.
        sup.start_node(node, &spawner).await.expect("start");
        assert!(
            !RAN.load(Ordering::SeqCst),
            "start_node returned after the shell's first poll; the take below would not race it"
        );
        let stolen = second.take().expect("the glue only probes");
        settle(|| node.has_exited()).await;
        assert!(
            !RAN.load(Ordering::SeqCst),
            "{}: the worker ran",
            node.name()
        );
        assert_eq!(
            first.take().expect("restored").0,
            1,
            "{}: the entry taken before the lost one is back in its slot",
            node.name()
        );
        assert_eq!(stolen.0, 2);
    }
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_lost_resource_restores_the_entries_already_taken() {
    // Frozen mock clock: the gate and spawn deadlines never fire.
    let _clock = MockDriver::get();

    let thread = std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });

    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        if thread.is_finished() {
            thread.join().expect("executor assertions failed");
            panic!("executor exited before completion");
        }
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(5));
    }
}
