//! A/B OTA bootloader for the RP2350 firmware.
//!
//! Boot ROM → this image → (swap DFU/ACTIVE if an update is pending) → ACTIVE app.
//! The swap is page-by-page and reversible: if the freshly-swapped app never calls
//! `mark_booted`, the next reset rolls back to the previous image.
//!
//! Adapted from `embassy/examples/boot/bootloader/rp`. RP2350 note: there is no
//! published rp235x bootloader example; the image-def comes from embassy-rp's
//! `binary-info` feature, but the jump-to-active and flash swap on Cortex-M33 are
//! unvalidated here — flash with BOOTSEL recovery ready.

#![no_std]
#![no_main]

use core::cell::RefCell;

use cortex_m_rt::{entry, exception};
use embassy_boot_rp::{BootLoader, BootLoaderConfig, WatchdogFlash};
use embassy_rp::flash::FLASH_BASE;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::Duration;
// defmt sink + panic integration; see Cargo.toml note. The defmt timestamp comes
// from embassy-time. embassy-rp's defmt calls need these symbols even though the
// bootloader logs nothing itself.
use {defmt_rtt as _, panic_probe as _};

const FLASH_SIZE: usize = 2 * 1024 * 1024;

#[entry]
fn main() -> ! {
    let p = embassy_rp::init(Default::default());

    // Watchdog-backed flash: if the swap hangs, the watchdog resets and the
    // bootloader retries (the swap is idempotent).
    let flash = WatchdogFlash::<FLASH_SIZE>::start(p.FLASH, p.WATCHDOG, Duration::from_secs(8));
    let flash: Mutex<NoopRawMutex, _> = Mutex::new(RefCell::new(flash));

    // Same flash handle backs the active / dfu / state regions (offsets differ).
    let config = BootLoaderConfig::from_linkerfile_blocking(&flash, &flash, &flash);
    let active_offset = config.active.offset();
    // BUFFER_SIZE = the RP flash sector (4 KiB); the bootloader swaps a page at a
    // time through a buffer of this size.
    let bl: BootLoader<4096> = BootLoader::prepare(config);

    // Jump to the (possibly just-swapped) ACTIVE application.
    unsafe { bl.load(FLASH_BASE as u32 + active_offset) }
}

#[unsafe(no_mangle)]
#[cfg_attr(target_os = "none", unsafe(link_section = ".HardFault.user"))]
unsafe extern "C" fn HardFault() -> ! {
    cortex_m::peripheral::SCB::sys_reset();
}

#[exception]
unsafe fn DefaultHandler(_: i16) -> ! {
    cortex_m::asm::udf();
}

// defmt's own `unwrap!`/`assert!` (used inside embassy-rp) call `_defmt_panic`.
// The bootloader uses no defmt macros itself, so define it explicitly.
#[defmt::panic_handler]
fn defmt_panic() -> ! {
    panic_probe::hard_fault();
}
