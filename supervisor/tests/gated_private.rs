mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_supervisor::{Supervisor, supervisor_graph};

static DONE: AtomicBool = AtomicBool::new(false);
static SAW: AtomicU32 = AtomicU32::new(0);

mod inner {
    use super::*;
    use embassy_supervisor::{Backed, TaskNode, dataflow};

    static ESTIMATE: Backed<AtomicU32> = Backed::new(AtomicU32::new(0));

    #[dataflow]
    pub async fn producer(node: &'static TaskNode) {
        node.writer(&ESTIMATE).store(7, Ordering::SeqCst);
        node.set_ready();
        let _ = node
            .run_cancellable_acked(core::future::pending::<()>())
            .await;
    }

    #[dataflow]
    pub async fn consumer(node: &'static TaskNode) {
        let est = node.open(&ESTIMATE).await;
        SAW.store(est.load(Ordering::SeqCst), Ordering::SeqCst);
        DONE.store(true, Ordering::SeqCst);
        let _ = node
            .run_cancellable_acked(core::future::pending::<()>())
            .await;
    }
}

supervisor_graph! {
    node PROD = Terminate, deps: [], task: inner::producer, disabled, discover;
    node CONS = Terminate, deps: [], task: inner::consumer, discover;
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    let fault = sup.run(&spawner).await;
    panic!("driver returned: {fault}");
}

#[test]
fn a_private_signal_still_resolves_its_producer() {
    support::run(
        |spawner| spawner.spawn(runner(spawner).unwrap()),
        &DONE,
        support::Clock::Frozen,
        10,
        || "gate did not resolve".to_owned(),
    );
    assert_eq!(SAW.load(Ordering::SeqCst), 7);

    // The derived entry names the signal as the call site wrote it.
    let names: Vec<&str> = CONS
        .reads()
        .iter()
        .flat_map(|t| t.iter())
        .map(|c| c.name())
        .collect();
    assert_eq!(names, ["ESTIMATE"]);
}
