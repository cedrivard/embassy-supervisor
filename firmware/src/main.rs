#![no_std]
#![no_main]

//! embassy-supervisor firmware (RP2350).
//!
//! Wires the `supervisor` lib to embassy tasks: builds the task graph and runs the
//! driver loop. Subsystems live in their own modules: `net` (USB-CDC-NCM), `http`
//! (elastic pool of keep-alive workers + the control/observability plane),
//! `heartbeat` (Pause-mode LED), `ota` (control-started A/B update via embassy-boot).

extern crate alloc;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use {defmt_rtt as _, panic_probe as _};

mod heap;
mod heartbeat;
mod http;
mod net;
mod ota;

use heartbeat::HEARTBEAT;
use http::HTTP;
use net::NET;

// Supervisor task graph -> `ALL_NODES` + `NODE_COUNT`. `heartbeat` is standalone;
// `http` and `ota` depend on `net`. `ota` is pre-disabled in `main`, control-started.
supervisor::task_graph! {
    &NET, &HEARTBEAT,
    &HTTP[0], &HTTP[1], &HTTP[2], &HTTP[3],
    &ota::OTA,
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // Init the HAL, then drop `Peripherals`: each subsystem steals the hardware it
    // needs (USB in net, PIN_25 in heartbeat, FLASH/WATCHDOG here), so `main` holds none.
    let _ = embassy_rp::init(Default::default());
    heap::init();
    defmt::info!(
        "boot: heap {}/{} B free",
        heap::free_bytes(),
        heap::HEAP_SIZE
    );

    spawner.spawn(defmt::unwrap!(watchdog_feed()));
    spawner.spawn(defmt::unwrap!(app_supervisor(spawner)));
    spawner.spawn(defmt::unwrap!(ota_confirm()));
}

/// The supervisor task: build the graph, bring everything up in dependency
/// order, then drive elastic-pool scaling and runtime control forever.
#[embassy_executor::task]
async fn app_supervisor(spawner: Spawner) {
    let sup = supervisor::Supervisor::new(&ALL_NODES)
        .expect("supervisor: dependency cycle")
        .with_pools(http::POOLS);
    // `ota` is stopped-at-boot: pre-disable so `start()` skips it; control `Activate`
    // (POST /api/ota or the dashboard start button) starts it.
    ota::OTA.set_disabled(true);
    sup.start(spawner).expect("supervisor: initial spawn failed");

    loop {
        // Race pool scaling against runtime control requests. `run_pools` never
        // returns; only a control command wakes the other arm.
        match select(sup.run_pools(spawner), supervisor::wait_control()).await {
            Either::First(()) => {}
            Either::Second(cmd) => sup.apply_control(cmd, spawner).await,
        }
    }
}

/// Feed the bootloader's 8 s watchdog (armed by `WatchdogFlash`, left running on
/// jump): a healthy app keeps feeding; a crashed/hung one stops -> reset -> the
/// bootloader rolls back an unconfirmed update.
#[embassy_executor::task]
async fn watchdog_feed() {
    let mut wd = embassy_rp::watchdog::Watchdog::new(unsafe {
        embassy_rp::peripherals::WATCHDOG::steal()
    });
    loop {
        wd.feed(embassy_time::Duration::from_secs(8)); // `feed` also sets the timeout
        embassy_time::Timer::after(embassy_time::Duration::from_secs(2)).await;
    }
}

/// Confirm the running image once the network is up. An update broken enough not to
/// reach here never calls `mark_booted`, so the bootloader rolls back on next reset.
#[embassy_executor::task]
async fn ota_confirm() {
    let stack = net::stack_ready().await;
    stack.wait_config_up().await;
    match ota::mark_booted() {
        Ok(()) => defmt::info!("ota: image confirmed"),
        Err(e) => defmt::warn!("ota: mark_booted failed: {}", e),
    }
}
