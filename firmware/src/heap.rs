//! Global heap allocator + free/used accounting.
//!
//! Heap is treated as a *budgeted* resource managed by the supervisor: each
//! heap-using subsystem has a known footprint, and starting/stopping subsystems
//! (admission control) keeps the total within this arena, so allocations are
//! infallible-but-safe. `free_bytes()` is surfaced by the HTTP status view so the
//! budget is observable.

use core::mem::MaybeUninit;
use embedded_alloc::LlffHeap;

#[global_allocator]
static HEAP: LlffHeap = LlffHeap::empty();

/// Heap arena size. Sized to hold the subsystem budgets at once; the
/// supervisor's start/stop is what keeps usage under it.
///
/// Two peaks bound this: serving (**~30 KB** — net + full 2-worker http pool at
/// ~4.6 KB/worker + one in-flight response; breakdown in `net.rs`/`http.rs`) and
/// the OTA decode (**~28 KB** — net + pool drained, `ruzstd` alone; window sizing
/// in `ota.rs`). They never coexist; serving is the high-water mark (each worker's
/// `tx` buffer holds a whole /api/tasks response so only one body String is ever
/// live — see `http.rs`), so 32 KB leaves ~2 KB margin at peak. The pool ceiling
/// (`POOL_MAX = 2`) is what keeps serving inside the arena: a third worker's
/// ~4.6 KB would blow it.
pub const HEAP_SIZE: usize = 32 * 1024;

/// Initialize the global allocator. Call once, early in `main`, before anything
/// allocates.
pub fn init() {
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
    // SAFETY: called exactly once at boot, before any allocation; HEAP_MEM is a
    // dedicated static never referenced elsewhere.
    unsafe { HEAP.init(&raw mut HEAP_MEM as usize, HEAP_SIZE) }
}

/// Bytes currently free in the arena (observable heap budget headroom).
pub fn free_bytes() -> usize {
    HEAP.free()
}
