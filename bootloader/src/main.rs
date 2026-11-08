#![no_std]
#![no_main]

use core::cell::RefCell;

use cortex_m_rt::{entry, exception};
use embassy_boot_rp::{BootLoader, BootLoaderConfig, WatchdogFlash};
use embassy_rp::flash::FLASH_BASE;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::Duration;
use {defmt_rtt as _, panic_probe as _};

/// The flash `WatchdogFlash` may address; matches `FLASH_SIZE` in
/// firmware/src/ota.rs. The partitions themselves come from memory.x.
const FLASH_SIZE: usize = 4 * 1024 * 1024;

#[entry]
fn main() -> ! {
    let p = embassy_rp::init(Default::default());

    let flash = WatchdogFlash::<FLASH_SIZE>::start(p.FLASH, p.WATCHDOG, Duration::from_secs(8));
    let flash: Mutex<NoopRawMutex, _> = Mutex::new(RefCell::new(flash));

    let config = BootLoaderConfig::from_linkerfile_blocking(&flash, &flash, &flash);
    let active_offset = config.active.offset();
    let bl: BootLoader<4096> = BootLoader::prepare(config);

    // SAFETY: `prepare` has finished any swap, so ACTIVE holds a whole image
    // linked at this address (firmware/memory.x FLASH); `load` points VTOR and
    // the stack at it and jumps.
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

#[defmt::panic_handler]
fn defmt_panic() -> ! {
    panic_probe::hard_fault();
}
