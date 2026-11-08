//! A hand-written driver loop over `pool_pass`: each pass applies the pools'
//! actions to completion and returns the instant the policy wants to be
//! re-evaluated at, so `select(wait_scale(), Timer::at(next))` grows the pool
//! under load and shrinks it once the idle surplus outlasts the cooldown.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph, wait_scale};
use embassy_time::{Duration, MockDriver, Timer};

supervisor_graph! {
    pool CREW = [OnDemand, OnDemand, OnDemand], deps: [], task: crew,
        policy: DeferredShrink::new(embassy_time::Duration::from_millis(100)), min: 0, max: 3;
}

static SPAWNS: AtomicU32 = AtomicU32::new(0);
/// Cleared by the script: every member drops its load.
static LOADED: AtomicBool = AtomicBool::new(true);
static DONE: AtomicBool = AtomicBool::new(false);

async fn until(cond: impl Fn() -> bool) {
    for _ in 0..100_000 {
        if cond() {
            return;
        }
        embassy_futures::yield_now().await;
    }
    assert!(cond(), "did not settle");
}

async fn yields(n: usize) {
    for _ in 0..n {
        embassy_futures::yield_now().await;
    }
}

fn running() -> usize {
    CREW.iter().filter(|m| m.is_running()).count()
}

/// Busy from its first poll until the script lifts the load, then idle.
async fn crew(node: &'static TaskNode) {
    SPAWNS.fetch_add(1, Ordering::SeqCst);
    node.mark_busy();
    let _ = node
        .run_cancellable_acked(async {
            until(|| !LOADED.load(Ordering::SeqCst)).await;
            node.mark_idle();
            core::future::pending::<()>().await
        })
        .await;
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner)
        .await
        .expect("an all-OnDemand pool: an empty wave");
    let drive = async {
        loop {
            let next = match sup.pool_pass(&spawner).await {
                Ok(next) => next,
                Err(fault) => return fault,
            };
            match next {
                Some(at) => {
                    select(wait_scale(), Timer::at(at)).await;
                }
                None => wait_scale().await,
            }
        }
    };
    let script = async {
        let clock = MockDriver::get();
        // Each member marks itself busy, so every grow leaves the pool
        // saturated and the next pass grows it again, up to `max`.
        until(|| CREW.iter().all(|m| m.is_running() && m.is_busy())).await;
        assert_eq!(SPAWNS.load(Ordering::SeqCst), 3, "one spawn per member");
        LOADED.store(false, Ordering::SeqCst);
        until(|| CREW.iter().all(|m| !m.is_busy())).await;
        yields(64).await;
        assert_eq!(running(), 3, "the idle surplus waits out the cooldown");
        // Nothing requests a scale from here on: only the instant `pool_pass`
        // returned wakes the loop, once per cooldown.
        clock.advance(Duration::from_millis(150));
        until(|| running() == 2).await;
        yields(64).await;
        assert_eq!(running(), 2, "one shrink per elapsed cooldown");
        clock.advance(Duration::from_millis(150));
        until(|| running() == 1).await;
        // One idle spare is the policy's dead band: no deadline is left.
        clock.advance(Duration::from_millis(500));
        yields(64).await;
        assert_eq!(running(), 1, "a single idle spare stays");
        assert_eq!(SPAWNS.load(Ordering::SeqCst), 3, "no regrow");
    };
    match select(drive, script).await {
        Either::First(fault) => panic!("pool pass faulted on {}", fault.node.name()),
        Either::Second(()) => DONE.store(true, Ordering::SeqCst),
    }
}

#[test]
fn a_hand_written_loop_scales_with_pool_pass() {
    let _clock = MockDriver::get();
    let thread = std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| spawner.spawn(driver(spawner).unwrap()));
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        if thread.is_finished() {
            thread.join().expect("executor assertions failed");
            panic!("executor exited before completion");
        }
        assert!(StdInstant::now() < deadline, "did not complete");
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
