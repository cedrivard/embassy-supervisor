//! Bench — a control-started compute load on **core 1**.
//!
//! Demonstrates the multi-core graph: this node is declared `executor: CORE1`,
//! so the core-0 supervisor spawns, stops, and restarts it on core 1's executor
//! through the graph's `SpawnerSlot` — placement stays supervisor-mediated (each
//! task lives on one core; nothing migrates).
//!
//! `Terminate` + `disabled`: it does nothing until a control `Activate`
//! (`POST /api/control?node=bench&op=start` or the dashboard button), then burns
//! CPU in yield-chunked slices until stopped. With the trace feature the effect
//! is directly visible: core 1's executor line jumps from idle to ~100% busy
//! (in-poll), while core 0's numbers are untouched — the whole point of putting
//! compute on the other core.
//!
//! Also the `exit:` demo: the worker returns its slice count, the shell
//! provides it into `BENCH_EXIT`, and `GET /api/bench` reads it with
//! `wait_take()` — completed-vs-cancelled is irrelevant here (the count is
//! meaningful either way), so the return type is a plain `u32`.

use embassy_supervisor::TaskNode;

/// One compute slice per poll, sized to a few hundred µs at 150 MHz: long
/// enough to dominate core 1's in-poll time, short enough to keep the poll
/// far under the 100 ms stall threshold (`yield_now` between slices keeps the
/// executor responsive to the shutdown signal).
const SLICE_ITERS: u32 = 50_000;

// A plain worker fn: the graph's `task:` declaration stamps the embassy shell. The
// node carries `executor: CORE1`, so the shell is spawned through that slot's
// SendSpawner — the future must be `Send`. Returns the slice count for the
// `exit: u32` slot; the shell's mark_exited() doubles as the ack, so the
// explicit ack_dropped is gone.
pub(crate) async fn bench_task(node: &'static TaskNode) -> u32 {
    // xorshift32: cheap, unoptimizable-away busywork (the state feeds back).
    let mut x: u32 = 0x1234_5678;
    let mut slices: u32 = 0;
    loop {
        if node.shutdown_requested() {
            return slices;
        }
        for _ in 0..SLICE_ITERS {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
        }
        // Keep the value observable so the loop cannot be optimized out.
        core::hint::black_box(x);
        slices = slices.wrapping_add(1);
        embassy_futures::yield_now().await;
    }
}
