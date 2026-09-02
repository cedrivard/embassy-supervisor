use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Observable, Stamped, Supervisor, TaskNode, dataflow, supervisor_graph};
use embassy_supervisor_observe::Counted;
use embassy_time::{Duration, MockDriver};

pub static S: Stamped<Counted<AtomicU32>> = Stamped::new(Counted::new(AtomicU32::new(0)));
static GO: AtomicBool = AtomicBool::new(false);
static PUT_DONE: AtomicBool = AtomicBool::new(false);

supervisor_graph! {
    node W = Terminate, deps: [], task: writer, discover;
}

#[dataflow]
async fn writer(node: &'static TaskNode) {
    while !GO.load(Ordering::SeqCst) {
        embassy_futures::yield_now().await;
    }
    node.put(&crate::S, 7);
    PUT_DONE.store(true, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    core::future::pending::<()>().await;
}

#[test]
fn a_stamped_signal_knows_how_old_its_value_is() {
    let clock = MockDriver::get();
    assert_eq!(S.age(), None, "never written");
    assert!(!S.is_fresh(Duration::from_secs(1)));
    assert!(S.read_fresh(Duration::from_secs(1)).is_none());
    assert_eq!(S.inner().inner().load(Ordering::SeqCst), 0);

    S.w().w().store(3, Ordering::SeqCst);
    assert_eq!(S.age(), Some(Duration::from_ticks(0)));
    assert!(S.is_fresh(Duration::from_ticks(0)));
    clock.advance(Duration::from_millis(100));
    assert_eq!(S.age(), Some(Duration::from_millis(100)));
    assert!(
        !S.is_fresh(Duration::from_millis(50)),
        "older than the bound"
    );
    assert!(S.is_fresh(Duration::from_millis(200)));
    assert_eq!(
        S.read_fresh(Duration::from_millis(200))
            .map(|c| c.r().load(Ordering::SeqCst)),
        Some(3)
    );
    let token = S.change_token();
    assert_eq!(token, 1, "forwarded: one counted write");
    clock.advance(Duration::from_secs(5));
    assert_eq!(S.change_token(), token, "time passing is not a write");

    // Through the node's verb: `put` goes through `w()`, so it stamps.
    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(runner(spawner).unwrap());
        });
    });
    GO.store(true, Ordering::SeqCst);
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !PUT_DONE.load(Ordering::SeqCst) {
        assert!(StdInstant::now() < deadline, "the writer never ran");
        std::thread::sleep(StdDuration::from_millis(2));
    }
    assert_eq!(S.age(), Some(Duration::from_ticks(0)), "stamped by the put");
    assert_eq!(S.r().r().load(Ordering::SeqCst), 7);
    assert_eq!(S.change_token(), 2);
}
