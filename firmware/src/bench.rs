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

use embassy_supervisor::TaskNode;

/// One compute slice per poll, sized to a few hundred µs at 150 MHz: long
/// enough to dominate core 1's in-poll time, short enough to keep the poll
/// far under the 100 ms stall threshold (`yield_now` between slices keeps the
/// executor responsive to the shutdown signal).
const SLICE_ITERS: u32 = 50_000;

// ─── Measurement-aid slice bodies (features `bench-excl` / `bench-mem`) ──────
//
// TEMPORARY instrumentation for the RP2350 idle-path investigation (see the
// README's "Note on RP2350"): both variants keep the same slice/yield structure
// but swap the slice body, turning bench into a calibrated cross-core
// interference generator for A/B-ing what core 1 traffic does to core 0:
//
// - `bench-excl`: an atomic `fetch_add` per iteration — an `ldrex`/`strex`
//   exclusive pair, approximating the pre-fix executor storm's exclusive
//   traffic (monitor events + SRAM writes).
// - `bench-mem`: an atomic load + store per iteration — plain `ldr`/`str` on
//   thumbv8m, the same SRAM traffic with NO exclusives. The excl/mem delta
//   isolates the exclusive-monitor effect from raw bus contention.
//
// The shared static keeps the two bodies identical except for the RMW-vs-
// load/store difference. On shutdown the task reports its achieved slice count
// and wall time so the interference rate can be computed per run.
#[cfg(all(feature = "bench-excl", feature = "bench-mem"))]
compile_error!("bench-excl and bench-mem are mutually exclusive: pick one per build");

#[cfg(any(feature = "bench-excl", feature = "bench-mem"))]
static STORM: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

// A plain worker fn: the graph's `task:` declaration stamps the embassy shell. The
// node carries `executor: CORE1`, so the shell is spawned through that slot's
// SendSpawner — the future must be `Send`.
pub(crate) async fn bench_task(node: &'static TaskNode) {
    // xorshift32: cheap, unoptimizable-away busywork (the state feeds back).
    #[cfg(not(any(feature = "bench-excl", feature = "bench-mem")))]
    let mut x: u32 = 0x1234_5678;
    let started = embassy_time::Instant::now();
    let mut slices: u32 = 0;
    loop {
        if node.shutdown_requested() {
            // Rate calibration for the interference runs: iters/s =
            // slices * SLICE_ITERS / (ms / 1000). Printed for every variant so
            // the default body doubles as the yield-rate (wake-coupling) probe.
            defmt::info!(
                "bench: {=u32} slices x {=u32} iters in {=u64} ms",
                slices,
                SLICE_ITERS,
                started.elapsed().as_millis()
            );
            node.ack_dropped();
            return;
        }
        #[cfg(feature = "bench-excl")]
        for _ in 0..SLICE_ITERS {
            STORM.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        #[cfg(feature = "bench-mem")]
        for _ in 0..SLICE_ITERS {
            let v = STORM.load(core::sync::atomic::Ordering::Relaxed);
            STORM.store(v.wrapping_add(1), core::sync::atomic::Ordering::Relaxed);
        }
        #[cfg(not(any(feature = "bench-excl", feature = "bench-mem")))]
        {
            for _ in 0..SLICE_ITERS {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
            }
            // Keep the value observable so the loop cannot be optimized out.
            core::hint::black_box(x);
        }
        slices = slices.wrapping_add(1);
        embassy_futures::yield_now().await;
    }
}
