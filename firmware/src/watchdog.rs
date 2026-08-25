use embassy_futures::select::{Either, select};
use embassy_supervisor::TaskNode;

use crate::GRAPH;

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
    loop {
        wd.feed(embassy_time::Duration::from_secs(8));
        for id in embassy_supervisor::trace::executors() {
            if id == 0 {
                continue;
            }
            if let Some((stalled, ticks)) = embassy_supervisor::trace::stalled_task(id, STALL_TICKS)
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
        match select(
            embassy_time::Timer::after(embassy_time::Duration::from_secs(2)),
            embassy_supervisor::wait_health(),
        )
        .await
        {
            Either::First(()) => {}
            Either::Second(ev) => match ev.kind {
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
        }
    }
}
