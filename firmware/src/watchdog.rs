use embassy_futures::select::{Either, select};
use embassy_supervisor::TaskNode;
use embassy_time::{Duration, Instant, Timer};

use crate::GRAPH;

/// Feed period. The bootloader armed the watchdog with an 8 s timeout.
const FEED_EVERY: Duration = Duration::from_secs(2);
const WD_TIMEOUT: Duration = Duration::from_secs(8);

/// `POST /api/reboot`: the watchdog task resets the chip through the watchdog.
#[cfg(feature = "enabled-if")]
pub(crate) static REBOOT: embassy_sync::signal::Signal<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    (),
> = embassy_sync::signal::Signal::new();

/// What woke the watchdog task besides its feed deadline.
enum Wake {
    Health(embassy_supervisor::HealthEvent),
    #[cfg(feature = "enabled-if")]
    Reboot,
}

/// Feed the hardware watchdog and detect stalled tasks.
pub(crate) async fn watchdog_task(
    node: &'static TaskNode,
    wd: &mut embassy_rp::watchdog::Watchdog,
) {
    node.set_detached(true);
    // Blocked-task detector (feature `trace`). Two complementary checks:
    // - `stalled_task`: an in-flight poll > 100 ms. For a stall on this task's OWN
    const STALL_TICKS: u32 = (embassy_time::TICK_HZ / 10) as u32;
    let mut warned = [0u32; 16];
    // The feed runs on its own deadline, not on a timer re-armed per wake, so
    // a stream of events cannot postpone it.
    let mut next_feed = Instant::now();
    loop {
        if Instant::now() >= next_feed {
            next_feed = Instant::now() + FEED_EVERY;
            feed(wd);
            for id in embassy_supervisor::trace::executors() {
                if id == 0 {
                    continue;
                }
                if let Some((stalled, ticks)) =
                    embassy_supervisor::trace::stalled_task(id, STALL_TICKS)
                {
                    defmt::warn!(
                        "trace: {} has been polling for {} ticks",
                        stalled.name(),
                        ticks
                    );
                }
            }
            for (n, w) in GRAPH.nodes.iter().flatten().zip(warned.iter_mut()) {
                let max = n.max_poll_ticks();
                if max > STALL_TICKS && max > *w {
                    *w = max;
                    defmt::warn!("trace: {} once held the executor {} ticks", n.name(), max);
                }
            }
        }
        // What the supervisor is doing right now survives a reset that cuts
        // this pass short; the next boot reports it.
        #[cfg(feature = "breadcrumb")]
        crate::bootflag::stash_crumb(embassy_supervisor::crumb_word());

        let Either::Second(wake) = select(Timer::at(next_feed), next_wake()).await else {
            continue;
        };
        match wake {
            Wake::Health(ev) => match ev.kind {
                embassy_supervisor::HealthKind::Stale { ticks } => defmt::warn!(
                    "liveness: {} stopped beating ({} ticks, still marked running)",
                    ev.node.name(),
                    ticks
                ),
                embassy_supervisor::HealthKind::Recovered => {
                    defmt::info!("liveness: {} is beating again", ev.node.name())
                }
                _ => {}
            },
            #[cfg(feature = "enabled-if")]
            Wake::Reboot => {
                defmt::info!("watchdog: reboot requested");
                wd.trigger_reset();
            }
        }
    }
}

/// Reload the hardware watchdog. Under `feed-gate`, only while every policed
/// node (`beat_timeout:`) is fresh: a stale one is left for the watchdog to
/// reset, and a stopped, paused or detached node is never stale.
fn feed(wd: &mut embassy_rp::watchdog::Watchdog) {
    #[cfg(feature = "feed-gate")]
    if !GRAPH.all_fresh() {
        let stale = GRAPH.stale_nodes().next().map_or("?", |n| n.name());
        defmt::error!("watchdog: feed withheld, stale: {}", stale);
        return;
    }
    wd.feed(WD_TIMEOUT);
}

/// The next health report or reboot request, whichever comes first.
async fn next_wake() -> Wake {
    let wake = async { Wake::Health(embassy_supervisor::wait_health().await) };
    #[cfg(feature = "enabled-if")]
    let wake = async {
        match select(wake, REBOOT.wait()).await {
            Either::First(w) => w,
            Either::Second(()) => Wake::Reboot,
        }
    };
    wake.await
}
