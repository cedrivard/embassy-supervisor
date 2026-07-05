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

// The supervised task graph — the single source of nodes, deps, pool, and order.
// `supervisor_graph!` generates the `static` nodes, the `HTTP` pool + `HTTP_POOL`,
// and the `GRAPH` bundle (node slots, dep table, elastic pools, and the compile-time
// topological order — a dependency cycle is a *compile* error) that `Supervisor::new`
// takes. `heartbeat` is standalone; `http` and `ota` depend on `net`; `ota` is
// disabled-at-boot (control-started).
embassy_supervisor::supervisor_graph! {
    executor HIGH;  // Interrupt-priority tier (SWI_IRQ_0)
    executor CORE1; // Core 1's thread executor (filled by core1_entry via spawn_core1)
    node NET = Terminate, deps: [], spawn: crate::net::net_task;
    node HEARTBEAT = Pause, deps: [], executor: HIGH, spawn: crate::heartbeat::heartbeat_task;
    pool HTTP = [Terminate, OnDemand, OnDemand, OnDemand], deps: [NET],
        spawn: crate::http::http_task,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(4)),
        min: 1, max: 4;
    node OTA = Terminate, deps: [NET], spawn: crate::ota::ota_task, disabled;
    node BENCH = Terminate, deps: [], executor: CORE1, spawn: crate::bench::bench_task, disabled;
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

    spawner.spawn(defmt::unwrap!(watchdog_feed()));
    spawner.spawn(defmt::unwrap!(app_supervisor(spawner)));
    spawner.spawn(defmt::unwrap!(ota_confirm()));
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

/// Feed the bootloader's 8 s watchdog (armed by `WatchdogFlash`, left running on
/// jump): a healthy app keeps feeding; a crashed/hung one stops -> reset -> the
/// bootloader rolls back an unconfirmed update.
#[embassy_executor::task]
async fn watchdog_feed() {
    let mut wd =
        embassy_rp::watchdog::Watchdog::new(unsafe { embassy_rp::peripherals::WATCHDOG::steal() });
    // Blocked-task detector (feature `trace`). Two complementary checks:
    // - `stalled_task`: an in-flight poll > 100 ms. On this single-executor
    //   firmware it can rarely fire (a blocked executor also blocks this task;
    //   it is here as the pattern for an ISR-priority observer), so additionally:
    // - `max_poll_ticks` watermark: post-hoc, names any node whose longest single
    //   poll exceeded the threshold — works even when observed after the fact.
    //   Warn only on increase to avoid log spam (16 slots cover this graph).
    const STALL_TICKS: u32 = (embassy_time::TICK_HZ / 10) as u32; // 100 ms
    let mut warned = [0u32; 16];
    loop {
        wd.feed(embassy_time::Duration::from_secs(8)); // `feed` also sets the timeout
        for id in embassy_supervisor::trace::executors() {
            if id == 0 {
                continue;
            }
            if let Some((node, ticks)) = embassy_supervisor::trace::stalled_task(id, STALL_TICKS) {
                defmt::warn!("trace: {} has been polling for {} ticks", node.name, ticks);
            }
        }
        for (node, w) in GRAPH.nodes.iter().flatten().zip(warned.iter_mut()) {
            let max = node.max_poll_ticks();
            if max > STALL_TICKS && max > *w {
                *w = max;
                defmt::warn!("trace: {} once held the executor {} ticks", node.name, max);
            }
        }
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
