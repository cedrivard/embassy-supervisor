//! A dependency-free stderr sink for the `log` backend on hosted targets.
//!
//! On a hosted target the supervisor's logs route through the `log` facade
//! (see `fmt.rs` — the `defmt` backend is embedded-only), so a simulator or
//! test binary only needs a `log::Log` installed to see bring-up lines and
//! stale-node reports. [`init_host_logging`] is that in one call: a static
//! logger writing `[uptime] LEVEL target: message` to stderr, formatted to
//! read like defmt's `timestamp-uptime` output from a probe, with no
//! dependency beyond `log` itself. Consumers who want filtering fancier
//! than a global level (per-module directives, env control) should install
//! `env_logger` or similar instead — the records carry this crate's module
//! paths as targets either way, so `RUST_LOG=embassy_supervisor=trace`
//! works there too.

// The one hosted-only corner of a no_std crate: gated to targets that have
// an operating system, where std is a given — minus `wasm32-unknown-unknown`
// (see lib.rs: no stderr, panicking clock; WASI targets stay in).
extern crate std;

use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

/// The instant of the `init_host_logging` call — the zero of the uptime
/// column, mirroring a firmware's boot.
static START: OnceLock<Instant> = OnceLock::new();

struct HostLogger;

static LOGGER: HostLogger = HostLogger;

impl log::Log for HostLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        // Level filtering is `log::set_max_level`'s job (compiled into the
        // macros' early-out); everything that reaches us is wanted.
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let uptime = START.get().map(|s| s.elapsed()).unwrap_or_default();
        // One write_all of one formatted line: std's stderr is unbuffered
        // and locked per call, so concurrent executor threads interleave
        // whole lines, not fragments.
        let line = std::format!(
            "[{:>5}.{:06}] {:5} {}: {}\n",
            uptime.as_secs(),
            uptime.subsec_micros(),
            record.level(),
            record.target(),
            record.args(),
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
    }

    fn flush(&self) {}
}

/// Call once from a simulator's or test's `main` before the supervisor
/// starts; records emitted earlier are dropped by the `log` facade. Errors
/// if a logger is already installed — including a second call — in which
/// case the existing logger and level are left untouched.
pub fn init_host_logging(max: log::LevelFilter) -> Result<(), log::SetLoggerError> {
    let _ = START.set(Instant::now());
    log::set_logger(&LOGGER)?;
    log::set_max_level(max);
    Ok(())
}
