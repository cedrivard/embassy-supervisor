use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use std::sync::atomic::{AtomicU32, Ordering};

use embassy_supervisor::{Coupling, Lease, Leased, TaskNode, dataflow, supervisor_graph};

pub static HANDLE: Leased<AtomicU32> = Leased::new(AtomicU32::new(7));

#[dataflow]
async fn producer(node: &'static TaskNode) {
    node.writer(&crate::HANDLE).store(7, Ordering::Relaxed);
}

#[dataflow]
async fn consumer(node: &'static TaskNode) {
    if let Some(h) = node.lease(&crate::HANDLE) {
        let _ = h.load(Ordering::Relaxed);
    }
}

supervisor_graph! {
    node PROD = Terminate, deps: [], task: producer, discover;
    node CONS = Terminate, deps: [], task: consumer, discover;
}

#[dataflow]
fn take(node: &'static TaskNode) -> Option<Lease<AtomicU32>> {
    node.lease(&crate::HANDLE)
}

fn names(tables: &[&[Coupling]]) -> Vec<&'static str> {
    tables
        .iter()
        .flat_map(|t| t.iter())
        .map(Coupling::name)
        .collect()
}

fn polled<F: Future>(fut: &mut core::pin::Pin<&mut F>) -> bool {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    matches!(fut.as_mut().poll(&mut cx), Poll::Ready(_))
}

#[test]
fn a_producer_drains_before_it_may_free() {
    assert_eq!(names(CONS.reads()), ["crate::HANDLE"], "`lease` is a read");
    assert_eq!(names(PROD.writes()), ["crate::HANDLE"]);
    let mut readers = Vec::new();
    GRAPH.readers_of(&CONS.reads()[0][0], &mut |_, n| readers.push(n.name()));
    assert_eq!(readers, ["cons"]);

    assert_eq!(HANDLE.leases(), 0);
    let a = take(&CONS).expect("open for business");
    let b = take(&CONS).expect("leases stack");
    assert_eq!(HANDLE.leases(), 2);
    assert_eq!(a.load(Ordering::Relaxed), 7, "Deref reaches the value");

    let mut drain = pin!(HANDLE.drain());
    assert!(!polled(&mut drain), "two leases are live");
    assert!(HANDLE.is_drained(), "but the signal is already closed");

    assert!(
        take(&CONS).is_none(),
        "a consumer asking mid-drain gets the honest answer, not a handle"
    );

    drop(b);
    assert_eq!(HANDLE.leases(), 1);
    assert!(!polled(&mut drain), "one still held");
    drop(a);
    assert_eq!(HANDLE.leases(), 0);
    assert!(polled(&mut drain), "the last drop released it");

    assert!(take(&CONS).is_none(), "still closed");
    HANDLE.reopen();
    assert!(!HANDLE.is_drained());

    let c = take(&CONS).expect("reopened");
    let d = take(&CONS).expect("two again");
    assert_eq!(HANDLE.leases(), 2);
    drop(c);
    assert_eq!(HANDLE.leases(), 1);
    drop(d);
    assert_eq!(HANDLE.leases(), 0);

    let mut idle = pin!(HANDLE.drain());
    assert!(polled(&mut idle), "nothing to wait for");
    HANDLE.reopen();

    assert_eq!(HANDLE.load(Ordering::Relaxed), 7);
    assert_eq!(HANDLE.leases(), 0);
}
