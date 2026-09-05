use embassy_supervisor::TaskNode;

const SLICE_ITERS: u32 = 50_000;

/// Run a CPU-bound benchmark task, returning the number of slices completed
/// before shutdown is requested.
pub(crate) async fn bench_task(node: &'static TaskNode) -> u32 {
    node.set_ready();
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
