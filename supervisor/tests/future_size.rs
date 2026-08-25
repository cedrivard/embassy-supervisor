use core::future::Future;
use core::mem::size_of_val;

use embassy_supervisor::{TaskNode, supervisor_graph};

supervisor_graph! {
    node SIZED = Terminate, deps: [], task: noop;
}

async fn noop(_node: &'static TaskNode) {}

/// Payload big enough that a doubling is unmistakable next to the handful of
/// bytes the race itself costs.
const PAYLOAD: usize = 4096;

/// A worker holding a large buffer **across** an await, so the buffer lands in
/// its state machine rather than on the stack.
///
/// Deliberately `-> impl Future` wrapping an `async` block rather than an
/// `async fn`: this test measures future *layout*, and the two forms are only
/// interchangeable as long as rustc lays them out identically — which is
/// exactly the kind of assumption a layout regression test must not bake in.
/// (`clippy::manual_async_fn` cannot know that, and CI lints with
/// `--all-targets`, so this `#[allow]` is load-bearing.)
#[allow(clippy::manual_async_fn)]
fn worker() -> impl Future<Output = usize> {
    async {
        let buf = [0u8; PAYLOAD];
        embassy_futures::yield_now().await;
        core::hint::black_box(&buf).len()
    }
}

#[test]
fn run_cancellable_stores_the_worker_once() {
    let base = size_of_val(&worker());
    assert!(base >= PAYLOAD, "worker future should carry the payload");

    let raced = size_of_val(&SIZED.run_cancellable(worker()));
    assert!(
        raced < base + base / 2,
        "run_cancellable holds the worker more than once: {raced} vs {base}"
    );

    let acked = size_of_val(&SIZED.run_cancellable_acked(worker()));
    assert!(
        acked < base + base / 2,
        "run_cancellable_acked holds the worker more than once: {acked} vs {base}"
    );

    let paused = size_of_val(&SIZED.run_pausable(worker()));
    assert!(
        paused < base + base / 2,
        "run_pausable holds the worker more than once: {paused} vs {base}"
    );

    let looped = size_of_val(&SIZED.run_pausable_loop(async || {
        let _ = worker().await;
    }));
    assert!(
        looped < base + base / 2,
        "run_pausable_loop holds the cycle future more than once: {looped} vs {base}"
    );
}
