#![no_std]
#![no_main]

extern crate alloc;

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
    unsafe { EXECUTOR_HIGH.on_interrupt() }
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

    interrupt::SWI_IRQ_0.set_priority(Priority::P2);
    HIGH.set(EXECUTOR_HIGH.start(interrupt::SWI_IRQ_0));

    embassy_supervisor::trace::set_core_id_fn(|| embassy_rp::pac::SIO.cpuid().read() as usize);

    #[allow(clippy::deref_addrof)]
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

    spawner.spawn(defmt::unwrap!(app_supervisor(spawner)));
}

#[embassy_executor::task]
async fn app_supervisor(spawner: Spawner) {
    let sup = embassy_supervisor::Supervisor::new(&GRAPH);
    defmt::panic!("supervisor: {}", sup.run(&spawner).await)
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
