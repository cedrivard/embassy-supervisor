// Every value-level clause takes a `#[cfg(...)]` gate. `all()` is the
// always-true predicate (the clause is active, through the gated emission
// path); `any()` is always false (rustc strips the clause, and the node
// behaves as if it were never written).
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node ON = Terminate, deps: [], task: worker,
        #[cfg(all())] slot_timeout: 250,
        #[cfg(all())] ack_timeout: 350,
        #[cfg(all())] beat_timeout: 100,
        #[cfg(all())] beat_window: 3,
        #[cfg(all())] disabled;
    node OFF = Terminate, deps: [], task: worker,
        #[cfg(any())] slot_timeout: 250,
        #[cfg(any())] ack_timeout: 350,
        #[cfg(any())] beat_timeout: 100,
        #[cfg(any())] disabled;
}

fn main() {
    assert!(ON.is_disabled(), "an `all()`-gated `disabled` latches");
    assert!(!OFF.is_disabled(), "an `any()`-gated `disabled` is stripped");
    assert_eq!(ON.slot_timeout(), embassy_time::Duration::from_millis(250));
    assert_eq!(
        ON.beat_timeout(),
        Some(embassy_time::Duration::from_millis(100))
    );
    assert_eq!(OFF.beat_timeout(), None, "the stripped node is unpoliced");
}
