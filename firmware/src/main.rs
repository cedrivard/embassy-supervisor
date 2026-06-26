#![no_std]
#![no_main]

//! embassy-supervisor firmware (RP2350).
//!
//! Wires the generalized `supervisor` lib to a set of embassy tasks. This file
//! builds the task graph and runs the supervisor's driver loop; each subsystem
//! lives in its own module: `net` (USB-CDC-NCM), `http` (an elastic pool of
//! keep-alive HTTP workers that is also the control/observability plane), and
//! `heartbeat` (Pause-mode LED).
//!
//! TODO: an `ota` subsystem (embassy-boot-rp A/B update) once the bootloader
//! crate lands.

extern crate alloc;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use {defmt_rtt as _, panic_probe as _};

mod heap;
mod heartbeat;
mod http;
mod net;

use heartbeat::HEARTBEAT;
use http::HTTP;
use net::NET;

// Declare the supervisor task graph once. Emits `ALL_NODES` + `NODE_COUNT`.
// `heartbeat` is standalone; the `http` pool's workers depend on `net`.
supervisor::task_graph! {
    &NET,
    &HEARTBEAT,
    &HTTP[0],
    &HTTP[1],
    &HTTP[2],
    &HTTP[3],
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // Init the HAL (sets up clocks, registers the peripheral singletons) then drop
    // the `Peripherals` — every subsystem self-acquires the hardware it needs by
    // stealing its peripheral (`USB` in net, `PIN_25` in heartbeat), so `main`
    // holds no handles and just starts the supervisor.
    let _ = embassy_rp::init(Default::default());
    heap::init();
    defmt::info!(
        "boot: heap {}/{} B free",
        heap::free_bytes(),
        heap::HEAP_SIZE
    );

    spawner.spawn(defmt::unwrap!(app_supervisor(spawner)));
}

/// The supervisor task: build the graph, bring everything up in dependency
/// order, then drive elastic-pool scaling and runtime control forever.
#[embassy_executor::task]
async fn app_supervisor(spawner: Spawner) {
    let sup = supervisor::Supervisor::new(&ALL_NODES)
        .expect("supervisor: dependency cycle")
        .with_pools(http::POOLS);
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
