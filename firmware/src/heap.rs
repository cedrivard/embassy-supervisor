use core::mem::MaybeUninit;
use embedded_alloc::LlffHeap;

#[global_allocator]
static HEAP: LlffHeap = LlffHeap::empty();

/// Total heap size in bytes.
pub const HEAP_SIZE: usize = 32 * 1024;

/// Initialize the global heap allocator.
pub fn init() {
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
    unsafe { HEAP.init(&raw mut HEAP_MEM as usize, HEAP_SIZE) }
}

/// Return the number of free bytes remaining in the heap.
pub fn free_bytes() -> usize {
    HEAP.free()
}
