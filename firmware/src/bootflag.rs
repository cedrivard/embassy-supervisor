//! State that outlives a reset, in the watchdog's scratch registers.
//!
//! RP235x keeps `WATCHDOG.SCRATCH0..7` across a watchdog or soft reset (only
//! power-on and the RUN pin clear them), so they carry a boot flag and the
//! last supervisor breadcrumb from one boot to the next, bootloader in between.
//! Neither the bootloader nor embassy-boot-rp/embassy-rp touch them. The boot
//! ROM reads 4..7 as a watchdog-boot vector only while SCRATCH4 holds its
//! magic (0xb007c0d3), which nothing here writes; 5 and 6 are otherwise free.

use embassy_rp::pac::WATCHDOG;

/// SCRATCH5 value that enables `bench` at boot. Anything else (0 after
/// power-on) leaves it disabled, as without `enabled-if`.
#[cfg(feature = "enabled-if")]
const BENCH_ON: u32 = 0xBE9C_0001;

/// `bench`'s `enabled_if:` predicate: is the boot flag set?
#[cfg(feature = "enabled-if")]
pub fn bench_enabled() -> bool {
    WATCHDOG.scratch5().read() == BENCH_ON
}

/// Set or clear the boot flag; takes effect at the next boot.
#[cfg(feature = "enabled-if")]
pub fn set_bench(on: bool) {
    WATCHDOG
        .scratch5()
        .write(|w| *w = if on { BENCH_ON } else { 0 });
}

/// Keep the latest `crumb_word()` where the next boot can read it.
#[cfg(feature = "breadcrumb")]
pub fn stash_crumb(w: u32) {
    WATCHDOG.scratch6().write(|v| *v = w);
}

/// The crumb the previous boot stashed (0: none). Clears the stash, so call
/// it once, at boot.
#[cfg(feature = "breadcrumb")]
pub fn take_prev_crumb() -> u32 {
    let w = WATCHDOG.scratch6().read();
    WATCHDOG.scratch6().write(|v| *v = 0);
    w
}
