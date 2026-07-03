//! Bench — a control-started compute load on **core 1**.
//!
//! Demonstrates the multi-core graph: this node is declared `executor: CORE1`,
//! so the core-0 supervisor spawns, stops, and restarts it on core 1's executor
//! through the graph's `SpawnerSlot` — placement stays supervisor-mediated (AMP:
//! each task lives on one core; nothing migrates).
//!
//! `Terminate` + `disabled`: it does nothing until a control `Activate`
//! (`POST /api/control?node=bench&op=start` or the dashboard button), then burns
//! CPU in yield-chunked slices until stopped. With the trace feature the effect
//! is directly visible: core 1's executor line jumps from idle to ~100% busy
//! (in-poll), while core 0's numbers are untouched — the whole point of putting
//! compute on the other core.

use embassy_supervisor::TaskNode;

/// One compute slice per poll, sized to a few hundred µs at 150 MHz: long
/// enough to dominate core 1's in-poll time, short enough to keep the poll
/// far under the 100 ms stall threshold (`yield_now` between slices keeps the
/// executor responsive to the shutdown signal).
const SLICE_ITERS: u32 = 50_000;

#[embassy_executor::task]
pub(crate) async fn bench_task(node: &'static TaskNode) {
    // xorshift32: cheap, unoptimizable-away busywork (the state feeds back).
    let mut x: u32 = 0x1234_5678;
    loop {
        if node.shutdown_requested() {
            node.ack_dropped();
            return;
        }
        for _ in 0..SLICE_ITERS {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
        }
        // Keep the value observable so the loop cannot be optimized out.
        core::hint::black_box(x);
        embassy_futures::yield_now().await;
    }
}
