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
use embassy_rp::interrupt;
use embassy_rp::interrupt::{InterruptExt, Priority};
use {defmt_rtt as _, panic_probe as _};

mod bench;
mod heap;
mod heartbeat;
mod http;
mod net;
mod ota;
mod watchdog;

// The supervised task graph — the single source of nodes, deps, pool, and order.
// `supervisor_graph!` generates the `static` nodes, the `HTTP` pool + `HTTP_POOL`,
// and the `GRAPH` bundle (node slots, dep table, elastic pools, and the compile-time
// topological order — a dependency cycle is a *compile* error) that `Supervisor::new`
// takes. `heartbeat` is standalone; `http` and `ota` depend on `net`; `ota` is
// disabled-at-boot (control-started).
embassy_supervisor::supervisor_graph! {
    executor HIGH;  // Interrupt-priority tier (SWI_IRQ_0)
    executor CORE1; // Core 1's thread executor (filled by core1_entry via spawn_core1)
    node WATCHDOG = Terminate, deps: [], spawn: crate::watchdog::watchdog_task;
    node NET = Terminate, deps: [], spawn: crate::net::net_task;
    node HEARTBEAT = Pause, deps: [], executor: HIGH, spawn: crate::heartbeat::heartbeat_task;
    pool HTTP = [Terminate, OnDemand, OnDemand, OnDemand], deps: [NET],
        spawn: crate::http::http_task,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: 1, max: 4;
    node OTA = Terminate, deps: [NET], spawn: crate::ota::ota_task, disabled;
    node BENCH = Terminate, deps: [], executor: CORE1, spawn: crate::bench::bench_task, disabled;
    node OTA_CONFIRM = Terminate, deps: [HTTP], spawn: crate::ota_confirm;
}

// The interrupt-priority executor backing the graph's `HIGH` slot. Its poll loop
// runs in the SWI_IRQ_0 handler at P2, preempting the thread executor.
// https://docs.rs/embassy-executor/latest/embassy_executor/struct.InterruptExecutor.html
static EXECUTOR_HIGH: embassy_executor::InterruptExecutor =
    embassy_executor::InterruptExecutor::new();

#[interrupt]
unsafe fn SWI_IRQ_0() {
    // SAFETY: called only from this vector, which EXECUTOR_HIGH owns after
    // `start()` — the contract `on_interrupt()` requires.
    unsafe { EXECUTOR_HIGH.on_interrupt() }
}

// Core 1's boot stack (the executor's tasks have their own statics; this is the
// entry/idle stack). `static mut` accessed exactly once, before core 1 starts.
static mut CORE1_STACK: embassy_rp::multicore::Stack<4096> = embassy_rp::multicore::Stack::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // Init the HAL; each subsystem steals the hardware it needs (USB in net,
    // PIN_25 in heartbeat, FLASH/WATCHDOG below) — `main` keeps only CORE1.
    let p = embassy_rp::init(Default::default());
    heap::init();

    // Bring up the HIGH tier and fill the graph's spawner slot BEFORE the
    // supervisor starts (an unfilled slot would fail heartbeat's spawn loudly).
    interrupt::SWI_IRQ_0.set_priority(Priority::P2);
    HIGH.set(EXECUTOR_HIGH.start(interrupt::SWI_IRQ_0));

    // Per-core preemption stacks for trace-nested: the crate is HAL-agnostic,
    // so the one-line core-id reader lives here (SIO.CPUID = current core).
    embassy_supervisor::trace::set_core_id_fn(|| embassy_rp::pac::SIO.cpuid().read() as usize);

    // Boot core 1: its executor publishes a SendSpawner into the graph's CORE1
    // slot.
    // SAFETY: CORE1_STACK is borrowed exactly once, here, before core 1 runs.
    let core1_stack = unsafe { &mut *&raw mut CORE1_STACK };
    embassy_rp::multicore::spawn_core1(p.CORE1, core1_stack, || {
        let executor =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(embassy_executor::Executor::new()));
        executor.run(|sp| CORE1.set(sp.make_send()))
    });
    defmt::info!(
        "boot: heap {}/{} B free",
        heap::free_bytes(),
        heap::HEAP_SIZE
    );

    // `watchdog` and `ota_confirm` are now graph nodes (WATCHDOG / OTA_CONFIRM),
    // brought up by the supervisor; only the supervisor task is spawned by hand.
    spawner.spawn(defmt::unwrap!(app_supervisor(spawner)));
}

/// The supervisor task: build the graph, bring everything up in dependency
/// order, then drive elastic-pool scaling and runtime control forever.
#[embassy_executor::task]
async fn app_supervisor(spawner: Spawner) {
    // Construction is infallible: the graph's topological order is computed at
    // compile time, so a dependency cycle would have been a compile error. `GRAPH`
    // carries the nodes, dep table, order, and the elastic pools.
    let sup = embassy_supervisor::Supervisor::new(&GRAPH);
    // `ota` is declared `disabled` (disabled-at-boot), so `start()` skips it; a
    // control `Activate` (POST /api/ota or the dashboard start button) starts it.
    sup.start(spawner)
        .await
        .expect("supervisor: initial spawn failed");

    loop {
        // Race pool scaling against runtime control requests. `run_pools` never
        // returns; only a control command wakes the other arm.
        match select(sup.run_pools(spawner), embassy_supervisor::wait_control()).await {
            Either::First(()) => {}
            Either::Second(cmd) => sup.apply_control(cmd, spawner).await,
        }
    }
}

/// Confirm the running image once the network is up — the `OTA_CONFIRM` node.
/// Started LAST (via `deps: [HTTP]`), so it only runs after the whole graph is up.
/// An update broken enough not to reach here never calls `mark_booted`, so the
/// bootloader rolls back on next reset. Runs once and exits.
#[embassy_executor::task]
async fn ota_confirm(_node: &'static embassy_supervisor::TaskNode) {
    let stack = net::stack_ready().await;
    stack.wait_config_up().await;
    match ota::mark_booted() {
        Ok(()) => defmt::info!("ota: image confirmed"),
        Err(e) => defmt::warn!("ota: mark_booted failed: {}", e),
    }
}
