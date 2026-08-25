use embassy_supervisor::{
    Backed, Coupling, Sig, TaskNode, dataflow, producer_of, supervisor_graph,
};
use embassy_supervisor_observe::{Counted, Observable};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as Cs;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;

pub static CHAN: Channel<Cs, u32, 4> = Channel::new();
pub static MUX: Mutex<Cs, u32> = Mutex::new(0);
pub static GATED_CHAN: Backed<Channel<Cs, u32, 4>> = Backed::new(Channel::new());

pub static COUNTED_CHAN: Counted<Channel<Cs, u32, 4>> = Counted::new(Channel::new());

#[dataflow]
async fn producer(node: &'static TaskNode) {
    // A bounded send is async and fallible-when-full, so the verb hands the
    // channel back rather than performing the write itself.
    node.beat_writer(&crate::CHAN).send(1).await;
    node.writer(&crate::COUNTED_CHAN).w().send(2).await;
    // A mutex yields a guard, which no value verb can express either.
    *node.writer(&crate::MUX).lock().await = 7;
    // The gated channel's producer: what `open` on the other side resolves.
    node.writer(&crate::GATED_CHAN).send(3).await;
}

#[dataflow]
async fn consumer(node: &'static TaskNode) {
    let v = node.reader(&crate::CHAN).receive().await;
    let _ = *node.reader(&crate::MUX).lock().await + v;
    // The gate runs `Gated::ensure`, then `Deref` gives the channel's own API.
    let _ = node.open(&crate::GATED_CHAN).await.receive().await;
}

supervisor_graph! {
    node PROD = Terminate, deps: [], task: producer, discover,
        writes: [crate::COUNTED_CHAN observed beat], beat_timeout: 500;
    node CONS = Terminate, deps: [], task: consumer, discover;
    node OTHER = Terminate, deps: [],
        reads: [crate::CHAN observed via it.len() as u32],
        writes: [crate::MUX];
}

pub trait Channels {
    fn offer(&'static self, s: Sig<Channel<Cs, u32, 4>>, v: u32) -> bool;
}

impl Channels for TaskNode {
    fn offer(&'static self, s: Sig<Channel<Cs, u32, 4>>, v: u32) -> bool {
        s.target.try_send(v).is_ok()
    }
}

#[dataflow(write(offer))]
fn offer_one(node: &'static TaskNode, v: u32) -> bool {
    node.offer(&crate::CHAN, v)
}

fn names(tables: &[&[Coupling]]) -> Vec<&'static str> {
    tables
        .iter()
        .flat_map(|t| t.iter())
        .map(Coupling::name)
        .collect()
}

#[test]
fn a_channel_and_a_mutex_are_couplings_like_any_other() {
    assert_eq!(
        names(PROD.writes()),
        [
            "crate::COUNTED_CHAN",
            "crate::CHAN",
            "crate::COUNTED_CHAN",
            "crate::MUX",
            "crate::GATED_CHAN"
        ]
    );
    assert_eq!(
        names(CONS.reads()),
        ["crate::CHAN", "crate::MUX", "crate::GATED_CHAN"],
        "`open` on a gated channel records a read like `reader` does"
    );
    assert_eq!(names(OTHER.reads()), ["crate::CHAN"]);
    assert_eq!(names(OTHER.writes()), ["crate::MUX"]);

    let counted = &PROD.writes()[0][0];
    let mut writers = Vec::new();
    GRAPH.writers_of(counted, &mut |_, n| writers.push(n.name()));
    assert_eq!(writers, ["prod"]);

    let gated = &CONS.reads()[0][2];
    assert_eq!(gated.name(), "crate::GATED_CHAN");
    assert_eq!(
        producer_of(&CONS, gated).map(|n| n.name()),
        Some("prod"),
        "the gated channel's producer, found by address"
    );
    let chan = &CONS.reads()[0][0];
    assert_eq!(
        producer_of(&CONS, chan).map(|n| n.name()),
        Some("prod"),
        "and the plain one's"
    );

    assert!(offer_one(&PROD, 9), "try_send into an empty channel");
    assert_eq!(CHAN.try_receive(), Ok(9));

    assert_eq!(COUNTED_CHAN.change_token(), 0);
    COUNTED_CHAN.w().try_send(1).expect("room");
    assert_eq!(COUNTED_CHAN.change_token(), 1);
    let _ = COUNTED_CHAN.r().try_receive();
    assert_eq!(
        COUNTED_CHAN.change_token(),
        1,
        "drained back to empty, and the token still does not go backwards — \
         which is what `len()` could not promise across a sweep"
    );
    assert_eq!(CHAN.len(), 0, "the same round trip, invisible in `len()`");
}
