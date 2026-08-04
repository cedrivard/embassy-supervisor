//! Layout regression: the `run_cancellable` combinators must store the worker's
//! state machine **once**.
//!
//! Written as `select(fut, wait_shutdown()).await` inside an `async fn`, they
//! stored it twice — once in the function's own frame (the by-value argument) and
//! once inside the select, because rustc does not overlap those slots
//! (rust-lang/rust#62958). That is invisible in a unit test of behaviour and very
//! visible in a binary: task storage is static, so a graph of `cancel` nodes paid
//! an extra copy of every worker future in `.bss`. A real graph measured +38 KB.
//!
//! The bound is deliberately generous (1.5x, against a doubling) so this fails on
//! the regression and not on a few bytes of bookkeeping drifting.

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
}
