//! Network over the USB cable: embassy-usb CDC-NCM -> embassy-net TCP/IP stack.
//!
//! No extra hardware — the host sees a USB network interface (`usb0` on Linux).
//!
//! Ownership model: the supervised `net` task owns the **entire** USB + network
//! bring-up and teardown. On start it allocates every buffer on the heap, builds
//! the USB device / CDC-NCM class / embassy-net stack, and drives all three
//! runners inside its own future (via `join`). On stop it drops them — releasing
//! the USB peripheral and **freeing net's whole heap budget**. So `net` is a real
//! budgeted resource: stopping it returns its memory, starting it re-allocates.
//!
//! The USB peripheral is **threaded from `main`** via the graph's `resources:`
//! clause (`USB_DEV: Peri<'static, USB>` in `main.rs`): main moves `p.USB` into
//! the macro-emitted slot (compile-time exclusive ownership — no `steal()`), the
//! generated shell lends this task `&mut Peri` for the run and restores it after
//! the task returns, so a control-plane stop/start re-takes the SAME peripheral
//! instance. The `Driver` is rebuilt from a `reborrow()` on each bring-up.
//!
//! One `static` bridges the gap between the task-owned objects and the rest of
//! the firmware:
//!
//! - `STACK` — the `Copy` stack handle, published for the `http` pool and the `ota`
//!   node through the safe [`StackCell`] wrapper (a `ResourceSlot`-style
//!   mutex-guarded `Cell`; see its docs for the core-0-only usage contract). The
//!   handle is lifetime-extended to `'static`, valid only while the task's
//!   backing buffers live. Sound because every stack user (`http`, `ota`) depends on
//!   `net`, so the supervisor tears them all down *before* `net` clears `STACK` and
//!   frees the backing (dependency-ordered teardown).
//!
//! Static IPv4 so no host DHCP server is needed: set your host's `usb0` to
//! `10.42.0.1/24` and reach the device at `10.42.0.61`.

use alloc::boxed::Box;
use embassy_futures::join::{join, join3};
use embassy_futures::select::select;
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_supervisor::TaskNode;
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

/// Number of concurrent sockets the stack can hold: one per http worker (the pool
/// ceiling) plus one for embassy-net's internal DNS socket (the `dns` feature
/// reserves a slot, used by reqwless's `HttpClient`). The OTA download's TCP socket
/// needs no extra slot — the supervisor drains the http pool before the OTA node
/// runs, so it reuses a freed worker slot (they never coexist).
pub const SOCKET_BUDGET: usize = crate::HTTP_MAX + 1;

// ─── Stack handle publication ──────────────────────────────────────────────

/// `ResourceSlot`-style safe wrapper for the published stack handle: a `Copy`
/// value behind `BlockingMutex<CriticalSectionRawMutex, Cell<..>>`, exposed as
/// safe `get`/`set` methods (compare `embassy_supervisor::ResourceSlot`, which
/// is the same storage with move-in/move-out semantics instead of copy-out).
///
/// `Stack` is `Copy` but neither `Send` nor `Sync` (it wraps `&RefCell`), so no
/// safe container can put it in a `static` — the `unsafe impl Sync` below is the
/// ONE place asserting the cross-thread contract, replacing the old `static mut`
/// and its per-access-site SAFETY comments:
/// - `get`/`set` themselves are data-race-free (critical section around a `Cell`
///   of a `Copy` value);
/// - the *handle* is only ever used on core 0 — `net`, `http`, and `ota` all run
///   there; core 1 (`bench`) never touches the network. Using it from core 1
///   would race embassy-net's internal `RefCell` borrows.
///
/// The handle's true lifetime is the net task's backing buffers; it is
/// lifetime-extended to `'static` for publication and the contract is upheld by
/// clearing it before the backing is freed (see the module-level invariant).
struct StackCell(
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

static STACK: StackCell = StackCell::new();

/// The current network stack, or `None` until `net` has brought it up.
pub fn try_stack() -> Option<Stack<'static>> {
    STACK.get()
}

/// Await the network stack becoming available. Dependents must use this rather
/// than assume `net` is up: the supervisor spawns nodes in topological order, but
/// the executor polls a spawn batch in *reverse* (last-spawned first), so a
/// dependent like `http` can be polled before `net`'s (synchronous) bring-up has
/// published the stack. Resolves immediately once it's up; only spins during the
/// brief startup window.
pub async fn stack_ready() -> Stack<'static> {
    loop {
        if let Some(s) = try_stack() {
            return s;
        }
        Timer::after(Duration::from_millis(2)).await;
    }
}

/// SAFETY: the caller must guarantee the backing of `s` outlives every use of the
/// published handle. Upheld by dependency-ordered teardown.
unsafe fn publish_stack(s: Stack<'_>) {
    // Lifetime-extend the `Copy` handle. `Stack<'a>`'s layout is independent of
    // `'a`, so this is a pure lifetime cast.
    let s: Stack<'static> = unsafe { core::mem::transmute(s) };
    STACK.set(Some(s));
}

fn unpublish_stack() {
    // Called on teardown after all dependents are down (dependency order).
    STACK.set(None);
}

// ─── The supervised net node ───────────────────────────────────────────────
//
// The `net` node `static` (root; everything depends on it) is generated by the
// `supervisor_graph!` invocation in `main.rs`; this module provides its task. The
// task owns the full USB + stack lifecycle, so stopping the node frees net's heap.

// Plain async worker (the graph's `task:` clause stamps its concrete
// `#[embassy_executor::task]` shell): the USB peripheral arrives as
// `&mut Peri<'static, USB>` out of the `USB_DEV` resource slot — the shell owns
// the `Peri` and restores it to the slot when this fn returns, so the next
// bring-up re-takes the same instance instead of stealing a fresh one.
pub(crate) async fn net_task(node: &'static TaskNode, usb: &mut embassy_rp::Peri<'static, USB>) {
    // ── Bring-up. All synchronous (no `.await`), so `STACK` is published on this
    // task's first poll. Dependents must still `stack_ready().await` rather than
    // assume net ran first — the executor polls a spawn batch last-first. ──
    // `reborrow()` scopes a fresh `Peri<'_, USB>` to this run: the `Driver` (and
    // everything built on it) dies at task exit, ending the reborrow so the
    // restored `Peri<'static>` is whole again for the next spawn.
    let driver = Driver::new(usb.reborrow(), Irqs);

    let mut config = Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("embassy-supervisor");
    config.product = Some("task supervisor (USB-net)");
    config.serial_number = Some("0001");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Every buffer on the heap (net's ~16 KB budget), owned by this task → freed
    // when it returns on teardown. Declared up front, before the objects that
    // borrow them: embassy ties each buffer's lifetime into the borrowing object's
    // type, so at scope-end (reverse-order) drop the buffers must outlive them.
    // `net_state` (the ~12 KB packet pool) + `resources` are the bulk. Release+LTO
    // constructs the Boxes in place (verified: largest poll frame in the binary is
    // ~2.9 KB — no 12 KB stack spike from the `Box::new` move).
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
        dns_servers: Default::default(), // embassy-net's heapless Vec, empty
    });
    // Fixed seed — fine for a USB-LAN link; use a hardware RNG in production.
    let seed = 0x0123_4567_89ab_cdef;
    let (stack, mut net_runner) = embassy_net::new(device, net_config, &mut resources, seed);

    // Publish for the http pool. SAFETY: torn down before this backing is freed.
    unsafe { publish_stack(stack) };

    // ── Serve until the supervisor tears us down. The three runners never return;
    // `ready` just logs once the link is up. `select` against `wait_shutdown`. ──
    let ready = async {
        stack.wait_config_up().await;
        if let Some(cfg) = stack.config_v4() {
            defmt::info!("net: up at {}", cfg.address);
        }
    };
    let serve = join(
        join3(usb_dev.run(), ncm_runner.run(), net_runner.run()),
        ready,
    );
    let _ = select(serve, node.wait_shutdown()).await;

    // ── Teardown: stop publishing, ack, then drop everything. The `Box`es, USB
    // device, and runners all drop here — USB is disabled and net's heap budget
    // (~the packet pool + socket storage + descriptors) is returned. ──
    unpublish_stack();
    node.ack_dropped();
}
