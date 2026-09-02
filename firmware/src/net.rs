embassy_supervisor::supervisor_fragment! {
    name: NET_FRAG;
    node NET = Terminate, task: crate::net::net_task,
        dataflow: [crate::net::publish_stack],
        provides: [NET_STACK],
        resources: [USB_DEV: embassy_rp::Peri<'static, embassy_rp::peripherals::USB>];
}

use alloc::boxed::Box;
use embassy_futures::join::{join, join3};
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_supervisor::{Backed, Lease, Leased, TaskNode};
use embassy_time::{Duration, Timer};
use embassy_usb::class::cdc_ncm::embassy_net::State as NetState;
use embassy_usb::class::cdc_ncm::{CdcNcmClass, State};
use embassy_usb::{Builder, Config};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

const MTU: usize = 1514;

/// Device static IP (host-side `usb0` should be 10.42.0.1/24).
const DEV_IP: Ipv4Address = Ipv4Address::new(10, 42, 0, 61);
const GW_IP: Ipv4Address = Ipv4Address::new(10, 42, 0, 1);
const PREFIX: u8 = 24;

/// Number of concurrent sockets the stack can hold: one per http worker (the
/// pool ceiling), plus one for embassy-net's internal DNS socket when the `dns`

pub const SOCKET_BUDGET: usize = crate::HTTP_MAX + cfg!(feature = "dns") as usize;

pub(crate) struct StackCell(
    embassy_sync::blocking_mutex::Mutex<
        embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
        core::cell::Cell<Option<Stack<'static>>>,
    >,
);

// SAFETY: see the struct docs — accesses are critical-section-guarded, and the
// contained handle is only used from core 0.
unsafe impl Sync for StackCell {}

impl StackCell {
    const fn new() -> Self {
        Self(embassy_sync::blocking_mutex::Mutex::new(
            core::cell::Cell::new(None),
        ))
    }
    fn get(&self) -> Option<Stack<'static>> {
        self.0.lock(core::cell::Cell::get)
    }
    fn set(&self, s: Option<Stack<'static>>) {
        self.0.lock(|c| c.set(s));
    }
}

/// Gated and leased, which are the two halves of the same dependency.
///
/// `Backed` is bring-up: reading through [`stack_ready`] waits for `net` to
/// assert readiness, which it does after publishing. `Leased` is teardown:
/// `net` counts the consumers holding the handle and drains them before it
/// frees the backing, so the module invariant above is checked rather than
/// argued. Both are `#[repr(C)]` with the wrapped value first, so the derived
/// write in [`publish_stack`], the module's own `STACK.set(..)` and a
static STACK: Backed<Leased<StackCell>> = Backed::new(Leased::new(StackCell::new()));

/// A leased handle to the network stack; the lease keeps the stack alive.
pub struct StackLease {
    stack: Stack<'static>,
    _hold: Lease<StackCell>,
}

impl core::ops::Deref for StackLease {
    type Target = Stack<'static>;
    fn deref(&self) -> &Stack<'static> {
        &self.stack
    }
}

/// The current network stack, or `None` until `net` has brought it up.
pub fn try_stack() -> Option<Stack<'static>> {
    STACK.get()
}

/// Wait for the network stack to be published and return a leased handle.
#[embassy_supervisor::dataflow]
pub async fn stack_ready(node: &'static TaskNode) -> Option<StackLease> {
    let cell = node.open(&crate::net::STACK).await.signal();
    loop {
        let hold = cell.lease()?;
        if let Some(stack) = cell.get() {
            return Some(StackLease { stack, _hold: hold });
        }
        drop(hold);
        Timer::after(Duration::from_millis(2)).await;
    }
}

/// The stack under a lease without waiting for readiness, for a caller that
/// copes with `None` rather than gating on it. `None` while `net` is down or
/// draining.
///
/// Reads through the caller's node, so an adopting caller
#[embassy_supervisor::dataflow]
pub fn lease_stack(node: &'static TaskNode) -> Option<StackLease> {
    let cell = node.reader(&crate::net::STACK);
    let hold = cell.lease()?;
    let stack = cell.get()?;
    Some(StackLease { stack, _hold: hold })
}

/// A bare claim on net's backing, for a consumer that already holds a `Stack`
pub(crate) fn hold() -> Option<Lease<StackCell>> {
    STACK.lease()
}

#[embassy_supervisor::dataflow]
unsafe fn publish_stack(node: &'static TaskNode, s: Stack<'_>) {
    let s: Stack<'static> = unsafe { core::mem::transmute(s) };
    node.writer(&crate::net::STACK).set(Some(s));
    // The spawn-time fan-out: the http pool's glue copies this out per member.
    crate::NET_STACK.provide(s);
}

fn unpublish_stack() {
    STACK.set(None);
}

pub(crate) async fn net_task(node: &'static TaskNode, usb: &mut embassy_rp::Peri<'static, USB>) {
    let driver = Driver::new(usb.reborrow(), Irqs);

    let mut config = Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("embassy-supervisor");
    config.product = Some("task supervisor (USB-net)");
    config.serial_number = Some("0001");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    let mut config_desc = Box::new([0u8; 256]);
    let mut bos_desc = Box::new([0u8; 256]);
    let mut control_buf = Box::new([0u8; 128]);
    let mut state = Box::new(State::new());
    let mut net_state = Box::new(NetState::<MTU, 4, 4>::new());
    let mut resources = Box::new(StackResources::<SOCKET_BUDGET>::new());

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_desc[..],
        &mut bos_desc[..],
        &mut [],
        &mut control_buf[..],
    );

    let our_mac = [0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC];
    let host_mac = [0x88, 0x88, 0x88, 0x88, 0x88, 0x88];
    let class = CdcNcmClass::new(&mut builder, &mut state, host_mac, 64);
    let mut usb_dev = builder.build();

    let (ncm_runner, device) = class.into_embassy_net_device::<MTU, 4, 4>(&mut net_state, our_mac);

    let net_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(DEV_IP, PREFIX),
        gateway: Some(GW_IP),
        dns_servers: Default::default(),
    });
    let seed = 0x0123_4567_89ab_cdef;
    let (stack, mut net_runner) = embassy_net::new(device, net_config, &mut resources, seed);

    STACK.reopen();
    unsafe { publish_stack(node, stack) };

    let ready = async {
        stack.wait_config_up().await;
        node.set_ready();
        if let Some(cfg) = stack.config_v4() {
            defmt::info!("net: up at {}", cfg.address);
        }
    };
    let serve = join(
        join3(usb_dev.run(), ncm_runner.run(), net_runner.run()),
        ready,
    );
    let _ = node.run_cancellable(serve).await;

    STACK.drain().await;
    unpublish_stack();
    node.ack_dropped();
}
