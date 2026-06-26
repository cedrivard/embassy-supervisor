//! Global heap allocator + free/used accounting.
//!
//! Heap is treated as a *budgeted* resource managed by the supervisor: each
//! heap-using subsystem has a known footprint, and starting/stopping subsystems
//! (admission control) keeps the total within this arena, so allocations are
//! infallible-but-safe. `free_bytes()` is surfaced by the HTTP status view so the
//! budget is observable.
//!
//! TODO: the OTA flow will free budget by draining subsystems before its large
//! firmware buffer.

use core::mem::MaybeUninit;
use embedded_alloc::LlffHeap;

#[global_allocator]
static HEAP: LlffHeap = LlffHeap::empty();

/// Heap arena size. Sized to hold the subsystem budgets at once; the
/// supervisor's start/stop is what keeps usage under it.
///
/// Measured peak is ~28.3 KB (net 15.5 KB + 4 http workers × 3 KB + one in-flight
/// response `String`), stable under `wrk -c12` on `/api/tasks` — only one response
/// `String` is ever live, since a worker holds it just across `send().await` and
/// the ~830 B body fits the 1 KB tx buffer, so the write never yields. 32 KB leaves
/// ~4.5 KB (~14%) headroom. NOTE: this margin assumes responses stay under the tx
/// buffer; a route whose body exceeds ~1 KB would make `send` yield and let
/// per-worker `String`s coexist (peak up to ~4×) — re-measure if you add one.
pub const HEAP_SIZE: usize = 32 * 1024;

/// Initialize the global allocator. Call once, early in `main`, before anything
/// allocates.
pub fn init() {
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
    // SAFETY: called exactly once at boot, before any allocation; HEAP_MEM is a
    // dedicated static never referenced elsewhere.
    unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }
}

/// Bytes currently free in the arena (observable heap budget headroom).
pub fn free_bytes() -> usize {
    HEAP.free()
}
