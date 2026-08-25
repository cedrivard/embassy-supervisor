use core::cell::RefCell;
use core::fmt::Write as _;

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use embassy_boot_rp::{BlockingFirmwareUpdater, FirmwareUpdaterConfig};
use embassy_net::Ipv4Address;
#[cfg(feature = "dns")]
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_rp::Peri;
use embassy_rp::flash::{Blocking, ERASE_SIZE, FLASH_BASE, Flash};
use embassy_rp::peripherals::FLASH;
use embassy_supervisor::{ControlOp, TaskNode};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_time::{Duration, Timer};
use embedded_io_async::Read as _;
use reqwless::client::HttpClient;
use reqwless::request::Method;
use ruzstd::decoding::StreamingDecoder;
use ruzstd::io::Read as _;

const FLASH_SIZE: usize = 2 * 1024 * 1024;

const SCRATCH_OFFSET: u32 = 0x1E0000;
const SCRATCH_LEN: u32 = 128 * 1024;

const DEFAULT_IP: Ipv4Address = Ipv4Address::new(10, 42, 0, 1);
const DEFAULT_PORT: u16 = 8000;
const DEFAULT_PATH: &str = "/fw.zst";

fn default_target() -> Target {
    Target {
        ip: DEFAULT_IP,
        port: DEFAULT_PORT,
        path: DEFAULT_PATH.to_string(),
    }
}

struct Target {
    ip: Ipv4Address,
    port: u16,
    path: String,
}

static TARGET: Mutex<CriticalSectionRawMutex, RefCell<Option<Target>>> =
    Mutex::new(RefCell::new(None));

/// Configure the OTA download target (IP, port, and firmware image path).
pub fn set_target(ip: &str, port: &str, path: &str) -> Result<(), &'static str> {
    let ip = if ip.is_empty() {
        DEFAULT_IP
    } else {
        parse_ip(ip).ok_or("bad ip")?
    };
    let port = if port.is_empty() {
        DEFAULT_PORT
    } else {
        port.parse().map_err(|_| "bad port")?
    };
    let path = if path.is_empty() { DEFAULT_PATH } else { path };
    TARGET.lock(|c| {
        *c.borrow_mut() = Some(Target {
            ip,
            port,
            path: path.to_string(),
        })
    });
    Ok(())
}

fn parse_ip(s: &str) -> Option<Ipv4Address> {
    let mut it = s.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some(Ipv4Address::new(a, b, c, d))
}

/// Parse-only resolver for reqwless's `Dns` bound, used when the `dns` feature
#[cfg(not(feature = "dns"))]
struct IpDns;

#[cfg(not(feature = "dns"))]
impl embedded_nal_async::Dns for IpDns {
    type Error = ();

    async fn get_host_by_name(
        &self,
        host: &str,
        _addr_type: embedded_nal_async::AddrType,
    ) -> Result<core::net::IpAddr, Self::Error> {
        parse_ip(host).map(core::net::IpAddr::V4).ok_or(())
    }

    async fn get_host_by_address(
        &self,
        _addr: core::net::IpAddr,
        _result: &mut [u8],
    ) -> Result<usize, Self::Error> {
        Err(())
    }
}

pub(crate) async fn ota_task(node: &'static TaskNode, flash: &mut Peri<'static, FLASH>) {
    node.set_detached(true);
    node.report_status("receiving image");
    match run(node, flash).await {
        Ok(()) => {
            node.report_status("image staged, resetting to swap");
            defmt::info!("ota: image staged to DFU, resetting to swap")
        }
        Err(e) => {
            node.report_status("transfer failed, resetting");
            defmt::error!("ota: failed: {} - resetting, no swap", e)
        }
    }
    Timer::after(Duration::from_millis(100)).await;
    cortex_m::peripheral::SCB::sys_reset();
}

async fn run(
    node: &'static TaskNode,
    flash: &mut Peri<'static, FLASH>,
) -> Result<(), &'static str> {
    // Fall back to the default target when started without one (e.g. the
    // dashboard's Activate button, which doesn't call set_target).
    let target = TARGET
        .lock(|c| c.borrow_mut().take())
        .unwrap_or_else(default_target);

    // Drain the http pool and wait for it to fully stop before opening the download
    // socket — else 2 workers + download = 3 would exceed the 2-socket budget.
    // Poll `is_running` (false only once teardown completes and the sockets are
    // freed), NOT `is_disabled` (set when the stop is merely *requested*, before the
    // workers actually exit). Deactivating the floor seeds the whole pool.
    embassy_supervisor::request_control(&crate::HTTP[0], ControlOp::Deactivate).await;
    while crate::HTTP.iter().any(|n| n.is_running()) {
        Timer::after(Duration::from_millis(20)).await;
    }

    let len = download_to_scratch(node, &target, flash.reborrow()).await?;
    defmt::info!("ota: downloaded {} B, draining net for the decode", len);
    // Already detached (ota_task's first act), so this net teardown won't cascade
    // back into us and no control op can interrupt the decode below. The
    // download's lease is dropped by now — it is scoped to the call above — and
    embassy_supervisor::request_control(&crate::NET, ControlOp::Deactivate).await;
    while crate::net::try_stack().is_some() {
        Timer::after(Duration::from_millis(20)).await;
    }
    defmt::info!(
        "ota: net down, {} B heap free, decoding",
        crate::heap::free_bytes()
    );
    apply(len, flash.reborrow())
}

fn transfer_buf(len: usize) -> Result<Vec<u8>, &'static str> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|_| "no heap for transfer buffers")?;
    buf.resize(len, 0);
    Ok(buf)
}

/// HTTP client (reqwless): GET the image and stream the body into scratch flash.
async fn download_to_scratch(
    node: &'static TaskNode,
    t: &Target,
    flash: Peri<'_, FLASH>,
) -> Result<usize, &'static str> {
    let held = crate::net::lease_stack(node).ok_or("net not up")?;
    let stack = *held;
    let state: Box<TcpClientState<1, 512, 2048>> = Box::new(TcpClientState::new());
    let tcp = TcpClient::new(stack, &state);
    #[cfg(feature = "dns")]
    let dns = DnsSocket::new(stack);
    #[cfg(not(feature = "dns"))]
    let dns = IpDns;
    let mut client = HttpClient::new(&tcp, &dns);

    let mut url = String::with_capacity(64);
    let _ = write!(url, "http://{}:{}{}", t.ip, t.port, t.path);

    let mut hdr = transfer_buf(1024)?;
    let mut req = client
        .request(Method::GET, &url)
        .await
        .map_err(|_| "http request")?;
    let resp = req
        .send(hdr.as_mut_slice())
        .await
        .map_err(|_| "http send")?;

    let mut reader = resp.body().reader();
    let mut chunk = transfer_buf(1024)?;
    let mut scratch = Scratch::new(flash);
    let mut written: u32 = 0;
    loop {
        let n = reader
            .read(&mut chunk[..])
            .await
            .map_err(|_| "http body read")?;
        if n == 0 {
            break;
        }
        scratch.write(written, &chunk[..n])?;
        written += n as u32;
    }
    Ok(written as usize)
}

type Blk<'a> = Flash<'a, FLASH, Blocking, FLASH_SIZE>;

struct Scratch<'a> {
    flash: Blk<'a>,
    erased_sectors: u32,
}

impl<'a> Scratch<'a> {
    fn new(p: Peri<'a, FLASH>) -> Self {
        Self {
            flash: Flash::new_blocking(p),
            erased_sectors: 0,
        }
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), &'static str> {
        if data.is_empty() {
            return Ok(());
        }
        let end = offset + data.len() as u32;
        if end > SCRATCH_LEN {
            return Err("image larger than scratch");
        }
        let needed = end.div_ceil(ERASE_SIZE as u32);
        while self.erased_sectors < needed {
            let s = SCRATCH_OFFSET + self.erased_sectors * ERASE_SIZE as u32;
            self.flash
                .blocking_erase(s, s + ERASE_SIZE as u32)
                .map_err(|_| "scratch erase")?;
            self.erased_sectors += 1;
        }
        self.flash
            .blocking_write(SCRATCH_OFFSET + offset, data)
            .map_err(|_| "scratch write")
    }
}

fn apply(compressed_len: usize, flash: Peri<'_, FLASH>) -> Result<(), &'static str> {
    let compressed = unsafe {
        core::slice::from_raw_parts(
            (FLASH_BASE as usize + SCRATCH_OFFSET as usize) as *const u8,
            compressed_len,
        )
    };

    let blk: Blk<'_> = Flash::new_blocking(flash);
    let shared: Mutex<NoopRawMutex, _> = Mutex::new(RefCell::new(blk));
    let config = FirmwareUpdaterConfig::from_linkerfile_blocking(&shared, &shared);
    let mut state_buf = [0u8; 1]; // == STATE::WRITE_SIZE
    let mut updater = BlockingFirmwareUpdater::new(config, &mut state_buf);

    let mut dec = StreamingDecoder::new(compressed).map_err(|_| "zstd header")?;
    let mut buf = [0u8; 512];
    let mut offset = 0usize;
    loop {
        let n = dec.read(&mut buf).map_err(|_| "zstd decode")?;
        if n == 0 {
            break;
        }
        updater
            .write_firmware(offset, &buf[..n])
            .map_err(|_| "dfu write")?;
        offset += n;
    }

    updater.mark_updated().map_err(|_| "mark_updated failed")
}

/// Confirm the running image so the bootloader keeps it instead of rolling back
/// on the next reset. Safe to call on a normal (non-updated) boot.
///
/// Called from the OTA_CONFIRM node — a *different* node from OTA, so the shared
/// FLASH cannot ride the `resources:` glue (one `Peri` provides one slot). Instead
/// it borrows the same `FLASH_DEV` slot **manually**: `take()` the peripheral, use
/// it, `restore()` it — the safe-slot equivalent of a scoped lock. An empty slot
/// means the OTA task currently owns the flash, and failing with an error here
/// is what keeps that from becoming a silent race.
pub fn mark_booted() -> Result<(), &'static str> {
    let mut p = crate::FLASH_DEV.take().ok_or("flash busy (ota running)")?;
    let result = {
        let blk: Blk<'_> = Flash::new_blocking(p.reborrow());
        let shared: Mutex<NoopRawMutex, _> = Mutex::new(RefCell::new(blk));
        let config = FirmwareUpdaterConfig::from_linkerfile_blocking(&shared, &shared);
        let mut state_buf = [0u8; 1];
        let mut updater = BlockingFirmwareUpdater::new(config, &mut state_buf);
        updater.mark_booted().map_err(|_| "mark_booted failed")
    };
    crate::FLASH_DEV.restore(p);
    result
}
