#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
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
            task: crate::bench::bench_task, exit: u32, disabled;

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
    unsafe { EXECUTOR_HIGH.on_interrupt() }
}

// Cross-core pend relay.
//
// An InterruptExecutor wakes by pending its IRQ in the local NVIC. A wake
// issued from core 1 would pend core 1's IRQ copy, where no executor runs,
// and the task would stall. The vector table is shared, so on core 1 the SWI
// handler rings core 0's doorbell; the bell handler re-pends the IRQ there.
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

#[interrupt]
unsafe fn SIO_IRQ_BELL() {
    embassy_rp::pac::SIO
        .doorbell_in_clr()
        .write(|w| w.set_doorbell_in_clr(1));
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
    unsafe { EXECUTOR_SUP.on_interrupt() }
}

/// The last fault `Supervisor::run` returned, for `/api/tasks`.
/// Written from the supervisor tier, read by the HTTP worker tier.
pub(crate) static LAST_FAULT: embassy_sync::blocking_mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    core::cell::Cell<Option<(&'static str, &'static str)>>,
> = embassy_sync::blocking_mutex::Mutex::new(core::cell::Cell::new(None));

fn record_fault(fault: &embassy_supervisor::NodeFault) {
    use embassy_supervisor::FaultKind;
    let kind = match fault.kind {
        FaultKind::ShutdownTimeout => "shutdown ack timeout",
        FaultKind::ExecutorSlotEmpty => "executor slot empty",
        FaultKind::ResourceMissing => "resource missing",
        FaultKind::ReadyDepTimeout { .. } => "ready dep timeout",
        FaultKind::Spawn(_) => "spawn refused",
        _ => "fault",
    };
    LAST_FAULT.lock(|c| c.set(Some((fault.node.name(), kind))));
}

static mut CORE1_STACK: embassy_rp::multicore::Stack<4096> = embassy_rp::multicore::Stack::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
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

    #[allow(clippy::deref_addrof)]
    let core1_stack = unsafe { &mut *&raw mut CORE1_STACK };
    // Core 0 handles doorbells; core 1 enables the interrupt SWIs so a
    // pend from there is relayed instead of lost.
    unsafe { interrupt::SIO_IRQ_BELL.enable() };
    embassy_rp::multicore::spawn_core1(p.CORE1, core1_stack, || {
        unsafe {
            interrupt::SWI_IRQ_0.enable();
            interrupt::SWI_IRQ_1.enable();
        }
        let executor =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(embassy_executor::Executor::new()));
        executor.run(|sp| CORE1.set(sp.make_send()))
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
    let spawner = unsafe { Spawner::for_current_executor() }.await;
    let sup = embassy_supervisor::Supervisor::new(&GRAPH);
    loop {
        // `run` returns on `ShutdownTimeout` (a wedged node). Report it and
        // re-enter; the wedged node stays marked running until it acks.
        let fault = sup.run(&spawner).await;
        defmt::error!("supervisor: {}", fault);
        record_fault(&fault);
    }
}

async fn ota_confirm(node: &'static embassy_supervisor::TaskNode) {
    node.set_detached(true);
    let Some(held) = net::stack_ready(node).await else {
        return;
    };
    held.wait_config_up().await;
    match ota::mark_booted() {
        Ok(()) => defmt::info!("ota: image confirmed"),
        Err(e) => defmt::warn!("ota: mark_booted failed: {}", e),
    }
}
