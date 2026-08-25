
use core::sync::atomic::{AtomicU32, Ordering};
use embassy_supervisor::{TaskNode, supervisor_graph};

pub struct Watch {
    id: AtomicU32,
}

impl Watch {
    pub const fn new() -> Self {
        Self {
            id: AtomicU32::new(0),
        }
    }
    pub fn msg_id(&self) -> u32 {
        self.id.load(Ordering::Relaxed)
    }
    pub fn subscribers(&self) -> u32 {
        71
    }
}

pub static ESTIMATE: Watch = Watch::new();
pub static SETPOINT: Watch = Watch::new();
pub static SAMPLES: [Watch; 2] = [Watch::new(), Watch::new()];
pub static TICKS: AtomicU32 = AtomicU32::new(0);

async fn producer(_node: &'static TaskNode) {}
async fn consumer(_node: &'static TaskNode) {}

supervisor_graph! {
    observe writes: it.msg_id();
    observe reads:  it.subscribers();

    node PRODUCER = Terminate, deps: [], task: producer,
        reads: [crate::SAMPLES[0] observed],
        writes: [
            crate::ESTIMATE observed,
            crate::TICKS observed via it.load(core::sync::atomic::Ordering::Relaxed),
        ];

    node CONSUMER = Terminate, deps: [PRODUCER], task: consumer,
        reads: [
            crate::ESTIMATE observed,
            #[cfg(any())]
            crate::SAMPLES[1] observed,
        ],
        writes: [crate::SETPOINT];
}

fn main() {
    assert_eq!(PRODUCER.writes()[0].len(), 2);
    assert_eq!(CONSUMER.reads()[0].len(), 1, "the cfg'd-out entry is absent");
    let w = PRODUCER.writes()[0];
    assert_eq!(w[0].name(), "crate::ESTIMATE");
    assert_eq!(w[0].observer().expect("marked observed").count(), 0);
    TICKS.store(7, Ordering::Relaxed);
    assert_eq!(w[1].observer().expect("marked observed").count(), 7);
    assert_eq!(PRODUCER.reads()[0][0].name(), "crate::SAMPLES[0]");
    assert_eq!(
        PRODUCER.reads()[0][0]
            .observer()
            .expect("marked observed")
            .count(),
        71
    );
    assert!(
        CONSUMER.writes()[0][0].observer().is_none(),
        "unmarked entries carry no accessor"
    );
}
