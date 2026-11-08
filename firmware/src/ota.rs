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
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use ruzstd::io::Read as _;

/// The flash `Flash` may address: 4 MiB (the Pico 2's part; the RedBoard has
/// 16 MiB), because `OTA_SCRATCH` ends at 2.125 MiB. memory.x asserts it.
/// A part that wraps at 2 MiB is refused before the first erase
/// (`flash_aliases_at_2mib`).
const FLASH_SIZE: usize = 4 * 1024 * 1024;

/// The compressed download's region as `(offset, len)` from the flash base:
/// `OTA_SCRATCH` in memory.x.
fn scratch_region() -> (u32, u32) {
    unsafe extern "C" {
        static __ota_scratch_start: u32;
        static __ota_scratch_end: u32;
    }
    let start = (&raw const __ota_scratch_start) as u32;
    (start, (&raw const __ota_scratch_end) as u32 - start)
}

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
/// is off. [`set_target`] accepts only what [`parse_ip`] parses and the default
/// is an address, so no lookup is ever needed; standing in for a `DnsSocket`
/// keeps smoltcp's DNS socket and its `StackResources` slot out of the binary
/// (`net::SOCKET_BUDGET`). `--features dns` links the real `DnsSocket` in its
/// place; `set_target` still accepts only an IPv4 literal, so the socket is
/// there to measure, not to resolve.
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
    // The request that started this task came over HTTP, which is the evidence
    // `ota-confirm` waits for, and `write_firmware` refuses to stage over an
    // unconfirmed swap (BadState). Confirm first.
    confirm(flash.reborrow())?;

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
    // download's lease ended with `download_to_scratch`, so net's drain does not
    // wait on this task.
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
    let (start, len) = scratch_region();
    if (start + len) as usize > 2 * 1024 * 1024 && flash_aliases_at_2mib() {
        return Err("flash smaller than 4 MiB");
    }
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
    if !resp.status.is_successful() {
        return Err("http status not 2xx");
    }

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

/// `true` when flash wraps at 2 MiB: the 256 bytes at offset 2 MiB read back as
/// the bootloader's first 256 bytes. `OTA_SCRATCH` ends past 2 MiB, so an erase
/// there would hit the bootloader on such a part.
fn flash_aliases_at_2mib() -> bool {
    const PROBE: usize = 256;
    const TWO_MIB: usize = 2 * 1024 * 1024;
    // SAFETY: both ranges are XIP-mapped flash inside FLASH_SIZE and are only read.
    let (low, high) = unsafe {
        (
            core::slice::from_raw_parts(FLASH_BASE as *const u8, PROBE),
            core::slice::from_raw_parts((FLASH_BASE as usize + TWO_MIB) as *const u8, PROBE),
        )
    };
    low == high
}

type Blk<'a> = Flash<'a, FLASH, Blocking, FLASH_SIZE>;

/// Streams the compressed download into OTA_SCRATCH, erasing each sector on
/// first touch (offsets only grow).
struct Scratch<'a> {
    flash: Blk<'a>,
    start: u32,
    len: u32,
    erased_sectors: u32,
}

impl<'a> Scratch<'a> {
    fn new(p: Peri<'a, FLASH>) -> Self {
        let (start, len) = scratch_region();
        Self {
            flash: Flash::new_blocking(p),
            start,
            len,
            erased_sectors: 0,
        }
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), &'static str> {
        if data.is_empty() {
            return Ok(());
        }
        let end = offset + data.len() as u32;
        if end > self.len {
            return Err("image larger than scratch");
        }
        let needed = end.div_ceil(ERASE_SIZE as u32);
        while self.erased_sectors < needed {
            let s = self.start + self.erased_sectors * ERASE_SIZE as u32;
            self.flash
                .blocking_erase(s, s + ERASE_SIZE as u32)
                .map_err(|_| "scratch erase")?;
            self.erased_sectors += 1;
        }
        self.flash
            .blocking_write(self.start + offset, data)
            .map_err(|_| "scratch write")
    }
}

fn apply(compressed_len: usize, flash: Peri<'_, FLASH>) -> Result<(), &'static str> {
    let (start, _) = scratch_region();
    // SAFETY: OTA_SCRATCH is XIP-mapped flash, `compressed_len` lies inside it
    // (`Scratch::write` bounds every write), and nothing writes it while the
    // slice lives: the download is over and the decode writes only DFU and
    // STATE, which memory.x keeps disjoint from it.
    let compressed = unsafe {
        core::slice::from_raw_parts(
            (FLASH_BASE as usize + start as usize) as *const u8,
            compressed_len,
        )
    };

    let blk: Blk<'_> = Flash::new_blocking(flash);
    let shared: Mutex<NoopRawMutex, _> = Mutex::new(RefCell::new(blk));
    let config = FirmwareUpdaterConfig::from_linkerfile_blocking(&shared, &shared);
    let mut state_buf = [0u8; 1]; // == STATE::WRITE_SIZE
    let mut updater = BlockingFirmwareUpdater::new(config, &mut state_buf);

    // `StreamingDecoder` boxes every decode error (`io_nostd::Error`), which
    // keeps ruzstd's error `Debug`/`Display` impls in the image and allocates
    // on the failure path; the frame decoder is driven by hand instead.
    let mut src: &[u8] = compressed;
    let mut dec = FrameDecoder::new();
    dec.init(&mut src).map_err(|_| "zstd header")?;
    let mut buf = [0u8; 512];
    let mut offset = 0usize;
    loop {
        while dec.can_collect() < buf.len() && !dec.is_finished() {
            let want = buf.len() - dec.can_collect();
            dec.decode_blocks(&mut src, BlockDecodingStrategy::UptoBytes(want))
                .map_err(|_| "zstd decode")?;
        }
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

/// Mark the running image booted, so the bootloader keeps it on the next reset
/// instead of rolling back. Writes nothing on a normal boot.
fn confirm(p: Peri<'_, FLASH>) -> Result<(), &'static str> {
    let blk: Blk<'_> = Flash::new_blocking(p);
    let shared: Mutex<NoopRawMutex, _> = Mutex::new(RefCell::new(blk));
    let config = FirmwareUpdaterConfig::from_linkerfile_blocking(&shared, &shared);
    let mut state_buf = [0u8; 1];
    let mut updater = BlockingFirmwareUpdater::new(config, &mut state_buf);
    updater.mark_booted().map_err(|_| "mark_booted failed")
}

/// `confirm` for `ota-confirm`, a different node from `ota`: the shared FLASH
/// cannot ride the `resources:` glue (one `Peri` fills one slot), so this
/// borrows the `FLASH_DEV` slot by hand with `take()`/`restore()`. An empty
/// slot means `ota` owns the flash and has already confirmed the image itself
/// (`run` confirms before it stages); that is `Ok(false)`.
pub fn mark_booted() -> Result<bool, &'static str> {
    let Some(mut p) = crate::FLASH_DEV.take() else {
        return Ok(false);
    };
    let result = confirm(p.reborrow());
    crate::FLASH_DEV.restore(p);
    result.map(|()| true)
}
