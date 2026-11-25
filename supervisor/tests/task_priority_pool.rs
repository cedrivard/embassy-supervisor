//! `task-priority` on pool members routed through a named executor slot, the
//! glue an interrupt tier or a second core uses: each member is stamped with
//! its own priority, a value set before the first start reaches that member
//! only, a later one waits for the member's next start, and a plain verb
//! leaves its caller alone.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use embassy_executor::{Metadata, Spawner};
use embassy_futures::join::join;
use embassy_supervisor::{DeferredShrink, Supervisor, TaskNode, supervisor_graph};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

supervisor_graph! {
    executor TIER;
    supervisor priority: 9;
    pool WORKERS = [Terminate, Terminate], deps: [], executor: TIER, priority: 7,
        task: member,
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 2, max: 2;
}

static GO: [Signal<CriticalSectionRawMutex, ()>; 2] = [Signal::new(), Signal::new()];
/// Each member's priority as its task last saw it.
static SEEN: [AtomicU8; 2] = [AtomicU8::new(0), AtomicU8::new(0)];
static STARTED: AtomicUsize = AtomicUsize::new(0);
static WAKES: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

async fn own_priority() -> u8 {
    Metadata::for_current_task().await.priority()
}

async fn member(node: &'static TaskNode) {
    let i = if core::ptr::eq(node, &WORKERS[0]) {
        0
    } else {
        1
    };
    SEEN[i].store(own_priority().await, Ordering::SeqCst);
    STARTED.fetch_add(1, Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(async {
            loop {
                GO[i].wait().await;
                SEEN[i].store(own_priority().await, Ordering::SeqCst);
                WAKES.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
}

fn seen() -> [u8; 2] {
    SEEN.each_ref().map(|s| s.load(Ordering::SeqCst))
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    assert_eq!(WORKERS[1].set_priority(9), Ok(()));
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("boot");
    support::settle("both members started", || {
        STARTED.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_eq!(seen(), [7, 9]);

    assert_eq!(WORKERS[0].set_priority(3), Ok(()));
    GO[0].signal(());
    GO[1].signal(());
    support::settle("both members woke", || WAKES.load(Ordering::SeqCst) == 2).await;
    assert_eq!(seen(), [7, 9]);
    assert_eq!(WORKERS[0].priority(), Some(3));
    assert_eq!(GRAPH.supervisor_priority, 9);

    let (res, during) = join(sup.teardown(), own_priority()).await;
    res.expect("both members ack");
    assert_eq!(during, 0);
    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn pool_members_take_their_priority_through_a_named_slot() {
    support::run(
        |spawner| {
            TIER.set(spawner.make_send());
            spawner.spawn(driver(spawner).unwrap());
        },
        &DONE,
        support::Clock::Frozen,
        10,
        || {
            format!(
                "started {}, wakes {}, seen {:?}",
                STARTED.load(Ordering::SeqCst),
                WAKES.load(Ordering::SeqCst),
                seen()
            )
        },
    );
}
