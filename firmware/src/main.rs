#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_rp::interrupt;
use embassy_rp::interrupt::{InterruptExt, Priority};
use {defmt_rtt as _, panic_probe as _};

mod bench;
#[cfg(any(feature = "breadcrumb", feature = "enabled-if"))]
mod bootflag;
mod heap;
mod heartbeat;
mod http;
mod net;
mod ota;
mod watchdog;

embassy_supervisor::compose_graph! {
    fragments: [NET_FRAG, HTTP_FRAG],
    graph: {
        default executor THREAD;
        executor HIGH;
        executor CORE1;
        node WATCHDOG = Terminate, task: crate::watchdog::watchdog_task,
            resources: [WD_DEV: embassy_rp::watchdog::Watchdog];

        node HEARTBEAT = Pause, executor: HIGH,
            task: crate::heartbeat::heartbeat_task,
            beat_timeout: 15000, discover,
            resources: [LED: embassy_rp::gpio::Output<'static>];

        node OTA = Terminate, deps: [NET ready], task: crate::ota::ota_task,
            dataflow: [crate::net::lease_stack],
            resources: [FLASH_DEV: embassy_rp::Peri<'static, embassy_rp::peripherals::FLASH>],
            slot_timeout: 10000,
            disabled;

        node BENCH = Terminate, deps: [HEARTBEAT ready bound], executor: CORE1,
            task: crate::bench::bench_task, exit: u32,
            #[cfg(not(feature = "enabled-if"))] disabled,
            #[cfg(feature = "enabled-if")] enabled_if: crate::bootflag::bench_enabled;

        node OTA_CONFIRM = Terminate, deps: [HTTP, NET ready], task: crate::ota_confirm,
            dataflow: [crate::net::stack_ready];
    }
}

static EXECUTOR_HIGH: embassy_executor::InterruptExecutor =
    embassy_executor::InterruptExecutor::new();

#[interrupt]
unsafe fn SWI_IRQ_0() {
    if on_core1() {
        return relay_to_core0(RELAY_HIGH);
    }
    // SAFETY: only this vector calls it, only on core 0 (core 1 returned above),
    // and core 0 enables the IRQ in `start()`, so never before it.
    unsafe { EXECUTOR_HIGH.on_interrupt() }
}

// Cross-core pend relay.
//
// An InterruptExecutor wakes by pending its IRQ in the local NVIC. A wake
// issued on the other core pends that core's IRQ copy, where the executor does
// not run, and the task would stall. The vector table is shared, so the SWI
// handler rings the other core's doorbell when it runs on the wrong core; the
// bell handler there re-pends the IRQ. `HIGH` and the supervisor run on core 0
// (`RELAY`), `CORE1` on core 1 (`RELAY_TO_CORE1`). Each core has its own
// doorbell register, so the two directions do not share a bit.
static RELAY: AtomicU32 = AtomicU32::new(0);
const RELAY_HIGH: u32 = 1 << 0;
const RELAY_SUP: u32 = 1 << 1;

fn on_core1() -> bool {
    embassy_rp::pac::SIO.cpuid().read() == 1
}

fn relay_to_core0(bit: u32) {
    RELAY.fetch_or(bit, Ordering::Release);
    embassy_rp::pac::SIO
        .doorbell_out_set()
        .write(|w| w.set_doorbell_out_set(1));
}

/// Core 1's executor. Interrupt-driven, so core 1 sleeps in `WFI` between
/// polls: a thread executor's `WFE` would be woken by every `SEV` that core 0's
/// thread pender broadcasts, one empty pass per core-0 wake.
static EXECUTOR_CORE1: embassy_executor::InterruptExecutor =
    embassy_executor::InterruptExecutor::new();
static RELAY_TO_CORE1: AtomicBool = AtomicBool::new(false);

#[interrupt]
unsafe fn SWI_IRQ_2() {
    if !on_core1() {
        // A wake issued on core 0 (a spawn or a stop signal from the
        // supervisor) pended core 0's copy: hand it to core 1.
        RELAY_TO_CORE1.store(true, Ordering::Release);
        embassy_rp::pac::SIO
            .doorbell_out_set()
            .write(|w| w.set_doorbell_out_set(1));
        return;
    }
    // SAFETY: only this vector calls it, only on core 1 (core 0 returned
    // above), and core 1 enables the IRQ in `start()`, so never before it.
    unsafe { EXECUTOR_CORE1.on_interrupt() }
}

#[interrupt]
unsafe fn SIO_IRQ_BELL() {
    embassy_rp::pac::SIO
        .doorbell_in_clr()
        .write(|w| w.set_doorbell_in_clr(1));
    if on_core1() {
        if RELAY_TO_CORE1.swap(false, Ordering::AcqRel) {
            interrupt::SWI_IRQ_2.pend();
        }
        return;
    }
    let bits = RELAY.swap(0, Ordering::AcqRel);
    if bits & RELAY_HIGH != 0 {
        interrupt::SWI_IRQ_0.pend();
    }
    if bits & RELAY_SUP != 0 {
        interrupt::SWI_IRQ_1.pend();
    }
}

/// The supervisor's own tier at P1: above the thread executor and below
/// the hardware handlers at P0. Keeps reporting when the thread tier is
/// hogged. The watchdog feeder stays in thread mode so a hang cannot hide
/// behind an unconditional feed.
static EXECUTOR_SUP: embassy_executor::InterruptExecutor =
    embassy_executor::InterruptExecutor::new();

#[interrupt]
unsafe fn SWI_IRQ_1() {
    if on_core1() {
        return relay_to_core0(RELAY_SUP);
    }
    // SAFETY: only this vector calls it, only on core 0 (core 1 returned above),
    // and core 0 enables the IRQ in `start()`, so never before it.
    unsafe { EXECUTOR_SUP.on_interrupt() }
}

/// The last fault the supervisor reported, for `/api/tasks`.
/// Written from the supervisor tier, read by the HTTP worker tier.
pub(crate) static LAST_FAULT: embassy_sync::blocking_mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    core::cell::Cell<Option<(&'static str, &'static str)>>,
> = embassy_sync::blocking_mutex::Mutex::new(core::cell::Cell::new(None));

pub(crate) fn record_fault(fault: &embassy_supervisor::NodeFault) {
    use embassy_supervisor::FaultKind;
    let kind = match fault.kind {
        FaultKind::ShutdownTimeout => "shutdown ack timeout",
        FaultKind::ExecutorSlotEmpty => "executor slot empty",
        FaultKind::ResourceMissing => "resource missing",
        FaultKind::ReadyDepTimeout { .. } => "ready dep timeout",
        FaultKind::Spawn(_) => "spawn refused",
        #[cfg(feature = "restart-intensity")]
        FaultKind::RestartStorm => "restart budget exhausted",
        _ => "fault",
    };
    LAST_FAULT.lock(|c| c.set(Some((fault.node.name(), kind))));
}

/// The crumb word the previous boot left in watchdog scratch, for
/// `/api/tasks`. Written once in `main`, before the graph starts.
#[cfg(feature = "breadcrumb")]
pub(crate) static PREV_CRUMB: AtomicU32 = AtomicU32::new(0);

/// The node name and phase a breadcrumb word names, if it is valid.
#[cfg(feature = "breadcrumb")]
pub(crate) fn decode_crumb(word: u32) -> Option<(&'static str, embassy_supervisor::CrumbPhase)> {
    let (phase, slot) = embassy_supervisor::decode_crumb(word)?;
    let node = GRAPH.nodes.get(usize::from(slot)).copied().flatten()?;
    Some((node.name(), phase))
}

static mut CORE1_STACK: embassy_rp::multicore::Stack<4096> = embassy_rp::multicore::Stack::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // What the supervisor was doing when the previous boot ended, read
    // before anything here can overwrite the stash.
    #[cfg(feature = "breadcrumb")]
    {
        let prev = bootflag::take_prev_crumb();
        PREV_CRUMB.store(prev, Ordering::Relaxed);
        match decode_crumb(prev) {
            Some((node, phase)) => defmt::info!("boot: previous crumb: {} {}", node, phase),
            None => defmt::info!("boot: no previous crumb ({=u32:#x})", prev),
        }
    }
    let p = embassy_rp::init(Default::default());
    heap::init();

    USB_DEV.provide(p.USB);
    WD_DEV.provide(embassy_rp::watchdog::Watchdog::new(p.WATCHDOG));
    FLASH_DEV.provide(p.FLASH);
    LED.provide(embassy_rp::gpio::Output::new(
        p.PIN_25,
        embassy_rp::gpio::Level::Low,
    ));
    for s in HTTP_STATS.iter() {
        s.provide(http::WorkerStats { served: 0 });
    }

    // graph default executor (thread-mode)
    THREAD.set(spawner.make_send());
    interrupt::SWI_IRQ_0.set_priority(Priority::P2);
    HIGH.set(EXECUTOR_HIGH.start(interrupt::SWI_IRQ_0));

    embassy_supervisor::trace::set_core_id_fn(|| embassy_rp::pac::SIO.cpuid().read() as usize);

    // SAFETY: the only reference to CORE1_STACK, taken once before core 1
    // starts. `&mut *&raw mut` makes it from a raw pointer, which keeps rustc's
    // `static_mut_refs` lint quiet.
    #[allow(clippy::deref_addrof)]
    let core1_stack = unsafe { &mut *&raw mut CORE1_STACK };
    // Each core handles its doorbell and enables the SWIs of the other core's
    // executors, so a pend issued on the wrong core is relayed instead of lost.
    // SAFETY: no critical section here is built on NVIC masks (embassy-rp's uses
    // PRIMASK and a spinlock), so unmasking cannot break one.
    unsafe {
        interrupt::SIO_IRQ_BELL.enable();
        interrupt::SWI_IRQ_2.enable();
    }
    embassy_rp::multicore::spawn_core1(p.CORE1, core1_stack, || {
        // SAFETY: as on core 0; on core 1 the SWI_IRQ_0/1 handlers only relay
        // to core 0, and the bell handler only re-pends SWI_IRQ_2.
        unsafe {
            interrupt::SWI_IRQ_0.enable();
            interrupt::SWI_IRQ_1.enable();
            interrupt::SIO_IRQ_BELL.enable();
        }
        // Lowest priority, so the relay handlers preempt a long `bench` poll.
        interrupt::SWI_IRQ_2.set_priority(Priority::P3);
        CORE1.set(EXECUTOR_CORE1.start(interrupt::SWI_IRQ_2));
        loop {
            cortex_m::asm::wfi();
        }
    });
    defmt::info!(
        "boot: heap {}/{} B free",
        heap::free_bytes(),
        heap::HEAP_SIZE
    );

    interrupt::SWI_IRQ_1.set_priority(Priority::P1);
    EXECUTOR_SUP
        .start(interrupt::SWI_IRQ_1)
        .spawn(defmt::unwrap!(app_supervisor()));
}

#[embassy_executor::task]
async fn app_supervisor() {
    // SAFETY: awaited inside this task, whose Context comes from EXECUTOR_SUP,
    // an embassy executor.
    let spawner = unsafe { Spawner::for_current_executor() }.await;
    let sup = embassy_supervisor::Supervisor::new(&GRAPH);
    // Before the first wave, so bring-up is recorded too.
    #[cfg(feature = "lifecycle-sink")]
    embassy_supervisor::set_lifecycle_sink(crate::http::record_lifecycle);
    loop {
        // `run` keeps driving through bring-up and activation faults (logged,
        // and recorded by the lifecycle sink) and returns only on a missed
        // shutdown ack or a refused restart. Report and re-enter; a wedged node
        // stays marked running until it acks.
        let fault = sup.run(&spawner).await;
        defmt::error!("supervisor: {}", fault);
        record_fault(&fault);
    }
}

/// Confirm the running image once it has shown it can take the next update:
/// the USB link is up and a worker has answered an HTTP request. Until then a
/// reset rolls a fresh update back. Runs last (`deps: [HTTP, NET ready]`),
/// detached, and exits once done.
async fn ota_confirm(node: &'static embassy_supervisor::TaskNode) {
    node.set_detached(true);
    let Some(held) = net::stack_ready(node).await else {
        return;
    };
    held.wait_link_up().await;
    // The lease covers only the link wait: held across the HTTP wait it would
    // keep `net`'s teardown waiting on this task.
    drop(held);
    http::first_response().await;
    match ota::mark_booted() {
        Ok(true) => defmt::info!("ota: image confirmed"),
        Ok(false) => defmt::info!("ota: image confirmed by ota"),
        Err(e) => defmt::warn!("ota: mark_booted failed: {}", e),
    }
}
