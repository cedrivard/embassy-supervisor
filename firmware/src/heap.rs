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
/// Two peaks bound this, balanced by design at ~28 KB:
/// - **Serving:** net (~15 KB; `SOCKET_BUDGET = POOL_MAX + 1 = 5` — 4 worker sockets
///   + 1 DNS) + 4 http workers × 3.07 KB + one in-flight response `String` ≈ ~28 KB.
///     (Only one `String` is ever live — small bodies fit the tx buffer so the write
///     never yields.)
/// - **OTA decode:** the OTA task drains the http pool *and* `net` (it decodes the
///   staged image from flash, not the network), so `ruzstd` decodes alone — ~28 KB
///   on-device for a `windowLog=11` image (measured 27.6 KB host peak; see the
///   `zstd-heapcheck` tool).
///
/// The pool size is chosen so the serving peak matches the OTA decode peak, and
/// `net` is drained for the decode so it never coexists with the decoder — that's
/// what keeps both peaks at ~28 KB and lets the arena be 32 KB (~4 KB margin). The
/// window is capped at 11 because ruzstd's heap ~doubles per `windowLog` (11→13 =
/// 28→71 KB) while the compressed image barely shrinks (firmware has few long-range
/// matches).
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
