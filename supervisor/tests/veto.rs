//! A distributed veto: several protection functions write one trip gate,
//! any one of them forces it, none can clear another's contribution, and a
//! stopped writer's trip stays until someone releases it on purpose.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use embassy_executor::Spawner;
use embassy_supervisor::{
    DeferredShrink, Observable, Supervisor, TaskNode, VetoGate, dataflow, supervisor_graph,
};

pub static TRIP: VetoGate<8> = VetoGate::new();

// Writer script by contributor slot: 0 idle, 1 assert, 2 release; the writer
// sets it back to 0 once done.
static CMD: [AtomicU8; 4] = [const { AtomicU8::new(0) }; 4];
static TRIPS: AtomicU32 = AtomicU32::new(0);
static RESETS: AtomicU32 = AtomicU32::new(0);
static NO_SLOT: AtomicBool = AtomicBool::new(false);
static STOP_BF0: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

supervisor_graph! {
    node OC = Terminate, deps: [], task: protector, discover, writes: [crate::TRIP veto];
    node DIFF = Terminate, deps: [], task: protector, discover, writes: [crate::TRIP veto];
    pool BF = [Terminate, Terminate], deps: [], task: protector, discover,
        writes: [crate::TRIP veto],
        policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)), min: 2, max: 2;
    node BREAKER = Terminate, deps: [], task: breaker, discover;
    node PLAIN = Terminate, deps: [], task: plain, discover;
}

async fn until(cond: impl Fn() -> bool) {
    while !cond() {
        embassy_futures::yield_now().await;
    }
}

#[dataflow]
async fn protector(node: &'static TaskNode) {
    let veto = node
        .veto(&crate::TRIP)
        .expect("declared with `veto`: a slot");
    let cmd = &CMD[usize::from(veto.slot())];
    let _ = node
        .run_cancellable_acked(async {
            loop {
                until(|| cmd.load(Ordering::SeqCst) != 0).await;
                match cmd.load(Ordering::SeqCst) {
                    1 => {
                        veto.assert();
                    }
                    _ => {
                        veto.release();
                    }
                }
                cmd.store(0, Ordering::SeqCst);
            }
        })
        .await;
}

#[dataflow]
async fn breaker(node: &'static TaskNode) {
    let gate = node.reader(&crate::TRIP);
    loop {
        gate.wait_asserted().await;
        TRIPS.fetch_add(1, Ordering::SeqCst);
        gate.wait_released().await;
        RESETS.fetch_add(1, Ordering::SeqCst);
    }
}

/// Reaches the gate through a verb without a declared `veto` slot: no handle.
#[dataflow]
async fn plain(node: &'static TaskNode) {
    NO_SLOT.store(node.veto(&crate::TRIP).is_none(), Ordering::SeqCst);
    let _ = node
        .run_cancellable_acked(core::future::pending::<()>())
        .await;
}

#[embassy_executor::task]
async fn runner(spawner: Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    // A stopped writer's contribution outlives it.
    until(|| STOP_BF0.load(Ordering::SeqCst)).await;
    sup.stop_node(&BF[0]).await.expect("acks");
    DONE.store(true, Ordering::SeqCst);
    // No driver loop: `run()` would re-spawn the stopped Terminate member,
    // and nothing here sends control requests.
    core::future::pending::<()>().await;
}

fn slot_of(node: &TaskNode) -> Option<u8> {
    node.writes()
        .iter()
        .flat_map(|t| t.iter())
        .find_map(|c| c.veto_slot())
}

#[test]
fn any_writer_trips_and_only_all_of_them_reset() {
    assert_eq!(slot_of(&OC), Some(0), "numbered in item order");
    assert_eq!(slot_of(&DIFF), Some(1));
    assert_eq!(slot_of(&BF[0]), Some(2), "one slot per pool member");
    assert_eq!(slot_of(&BF[1]), Some(3));
    assert_eq!(slot_of(&BREAKER), None, "a reader holds no slot");
    assert!(!TRIP.is_asserted());

    std::thread::spawn(|| {
        let executor: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        executor.run(|spawner| {
            spawner.spawn(runner(spawner).unwrap());
        });
    });
    let deadline = StdInstant::now() + StdDuration::from_secs(10);
    let wait_for = |what: &str, cond: &dyn Fn() -> bool| {
        while !cond() {
            assert!(
                StdInstant::now() < deadline,
                "{what} did not resolve (contributors={:#b}, trips={}, resets={})",
                TRIP.contributors(),
                TRIPS.load(Ordering::SeqCst),
                RESETS.load(Ordering::SeqCst),
            );
            std::thread::sleep(StdDuration::from_millis(2));
        }
    };
    let tell = |slot: usize, cmd: u8| {
        CMD[slot].store(cmd, Ordering::SeqCst);
        wait_for("writer acted", &|| CMD[slot].load(Ordering::SeqCst) == 0);
    };
    let trips = || TRIPS.load(Ordering::SeqCst);
    let resets = || RESETS.load(Ordering::SeqCst);

    wait_for("bring-up", &|| NO_SLOT.load(Ordering::SeqCst));
    assert!(
        NO_SLOT.load(Ordering::SeqCst),
        "no declared slot, no handle"
    );

    // ── any one contributor trips the gate ──────────────────────────────
    tell(0, 1);
    wait_for("trip", &|| trips() == 1);
    assert!(TRIP.is_asserted());
    assert_eq!(TRIP.contributors(), 0b0001);
    tell(1, 1);
    assert_eq!(TRIP.contributors(), 0b0011);
    assert_eq!(trips(), 1, "already asserted: no second flip");

    // ── releasing one contributor does not clear another's trip ─────────
    tell(0, 2);
    std::thread::sleep(StdDuration::from_millis(10));
    assert!(TRIP.is_asserted(), "DIFF still holds it");
    assert_eq!(resets(), 0);
    tell(1, 2);
    wait_for("reset", &|| resets() == 1);
    assert!(!TRIP.is_asserted());

    // ── a stopped writer's trip stays until released on purpose ─────────
    tell(2, 1);
    wait_for("second trip", &|| trips() == 2);
    STOP_BF0.store(true, Ordering::SeqCst); // hand BF[0] to the runner to stop
    wait_for("BF[0] stopped", &|| DONE.load(Ordering::SeqCst));
    assert!(!BF[0].is_running());
    assert!(
        TRIP.is_asserted(),
        "fail-safe: the dead protector keeps the trip"
    );
    assert_eq!(TRIP.contributors(), 0b0100);
    assert!(
        TRIP.release_slot(2),
        "the application releases it explicitly"
    );
    wait_for("reset after release", &|| resets() == 2);

    // ── the token counts flips, not writes ──────────────────────────────
    assert_eq!(TRIP.change_token(), 4, "trip, reset, trip, reset");
}
