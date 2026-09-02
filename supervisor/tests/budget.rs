//! A `divisible` resource in a graph: an allocator provides and divides it,
//! holders claim shares, and the supervisor gives a share back whenever its
//! holder stops — cleanly, by missing its ack, or not at all when it parks.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{Claimant, FairShare, FaultKind, Supervisor, TaskNode, supervisor_graph};
use embassy_time::{Duration, Instant, MockDriver};

static REBALANCES: AtomicU32 = AtomicU32::new(0);
/// What each holder's own loop last saw its grant as, by slot.
static SEEN: [AtomicU32; 5] = [const { AtomicU32::new(u32::MAX) }; 5];
static DONE: AtomicBool = AtomicBool::new(false);

supervisor_graph! {
    node ALLOC = Terminate, deps: [], task: allocator, provides: [POWER];
    node S1 = Terminate, deps: [ALLOC], task: session(30), resources: [POWER: divisible];
    node S2 = Terminate, deps: [ALLOC], task: session(30), resources: [POWER: divisible];
    node WEDGED = Terminate, deps: [ALLOC], task: wedged, resources: [POWER: divisible],
        ack_timeout: 50;
    node PARKED = Pause, deps: [ALLOC], task: parked, resources: [POWER: divisible];
    node LATE = Terminate, deps: [], task: session(10), resources: [POWER: divisible],
        disabled;
}

async fn allocator(node: &'static TaskNode) {
    POWER.provide(80);
    let _ = node
        .run_cancellable_acked(async {
            loop {
                POWER.wait_change().await;
                POWER.rebalance(&FairShare, Instant::now());
                REBALANCES.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
}

async fn watch_grant(power: Claimant) {
    let mut seen = power.grant();
    loop {
        SEEN[usize::from(power.slot())].store(seen, Ordering::SeqCst);
        seen = power.wait_grant_change(seen).await;
    }
}

async fn session(node: &'static TaskNode, power: Claimant, want: u32) {
    power.want(want);
    let _ = node.run_cancellable_acked(watch_grant(power)).await;
}

/// Never acks: the supervisor has to take its share back itself.
async fn wedged(_node: &'static TaskNode, power: Claimant) {
    power.want(20);
    watch_grant(power).await;
}

async fn parked(node: &'static TaskNode, power: Claimant) {
    power.want(20);
    node.run_pausable_loop(async || {
        watch_grant(power).await;
    })
    .await
}

async fn settle(mut f: impl FnMut() -> bool) {
    for _ in 0..20_000 {
        if f() {
            return;
        }
        embassy_futures::yield_now().await;
    }
}

fn grants() -> [u32; 5] {
    core::array::from_fn(|i| POWER.grant(i as u8))
}

fn seen() -> [u32; 5] {
    core::array::from_fn(|i| SEEN[i].load(Ordering::SeqCst))
}

#[embassy_executor::task]
async fn driver(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");

    // 30 + 30 + 20 + 20 over 80: proportional, 24/24/16/16.
    settle(|| grants() == [24, 24, 16, 16, 0]).await;
    assert_eq!(
        grants(),
        [24, 24, 16, 16, 0],
        "after {} rebalances",
        REBALANCES.load(Ordering::SeqCst)
    );
    settle(|| seen()[..4] == [24, 24, 16, 16]).await;
    assert_eq!(
        &seen()[..4],
        &[24, 24, 16, 16],
        "every holder's loop saw its grant"
    );

    // A clean stop gives the share back, and the survivors regrow.
    sup.deactivate(&S1).await.expect("S1 acks");
    assert_eq!(POWER.want_of(0), 0, "released on the ack");
    settle(|| grants() == [0, 30, 20, 20, 0]).await;
    assert_eq!(grants(), [0, 30, 20, 20, 0]);

    // An unprovided budget is a missing resource, like any other slot.
    embassy_supervisor::ResourceGate::clear(&POWER);
    let err = sup
        .start_node(&LATE, &spawner)
        .await
        .expect_err("no capacity: fail-closed");
    assert!(
        matches!(err.kind, FaultKind::ResourceMissing),
        "an unprovided budget is a missing resource: {err}"
    );
    POWER.provide(80);
    sup.start_node(&LATE, &spawner)
        .await
        .expect("provided: starts");
    settle(|| grants() == [0, 30, 20, 20, 10]).await;
    assert_eq!(
        grants(),
        [0, 30, 20, 20, 10],
        "a late holder joins at its want"
    );

    // A holder that never acks is faulted, and its share released for it.
    let err = sup.deactivate(&WEDGED).await.expect_err("never acks");
    assert!(matches!(err.kind, FaultKind::ShutdownTimeout), "{err}");
    assert_eq!(
        (POWER.want_of(2), POWER.grant(2)),
        (0, 0),
        "the supervisor released the wedged holder's share"
    );
    settle(|| grants() == [0, 30, 0, 20, 10]).await;
    assert_eq!(grants(), [0, 30, 0, 20, 10]);

    // A parked holder keeps what it claimed.
    sup.stop_node(&PARKED).await.expect("parks");
    assert!(!PARKED.is_running());
    assert_eq!(
        POWER.grant(3),
        20,
        "a Pause ack keeps the claim, like it keeps its resources"
    );

    // `provides:` on a budget: the allocator's stop empties it.
    sup.stop_node(&ALLOC).await.expect("acks");
    assert_eq!(POWER.capacity(), 0, "cleared on the provider's ack");
    assert!(!embassy_supervisor::ResourceGate::is_filled(&POWER));

    DONE.store(true, Ordering::SeqCst);
}

#[test]
fn a_divisible_resource_is_released_when_its_holder_stops() {
    let clock = MockDriver::get();
    assert_eq!(
        (POWER.slots(), POWER.capacity()),
        (5, 0),
        "five holders, one slot each, unprovided until the allocator runs"
    );
    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(driver(spawner).unwrap());
        });
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    while !DONE.load(Ordering::SeqCst) {
        assert!(
            StdInstant::now() < deadline,
            "timed out: grants={:?} seen={:?} rebalances={}",
            grants(),
            seen(),
            REBALANCES.load(Ordering::SeqCst)
        );
        clock.advance(Duration::from_millis(10));
        std::thread::sleep(StdDuration::from_millis(2));
    }
}
