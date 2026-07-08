//! Bootloader watchdog feeder + trace-based stall detector, as a **detached**
//! supervised node.

use embassy_supervisor::TaskNode;

use crate::GRAPH;

/// Feed the bootloader's 8 s watchdog (armed by `WatchdogFlash`, left running on
/// jump): a healthy app keeps feeding; a crashed/hung one stops -> reset -> the
/// bootloader rolls back an unconfirmed update.
///
/// A **detached** node ([`TaskNode::set_detached`]): the supervisor starts it once
/// and never tears it down or respawns it — it must keep feeding the watchdog for the
/// entire life of the app, independent of any teardown/respawn cycle.
///
/// A plain worker fn (`task:` in the graph): the `Watchdog` driver arrives moved
/// in from `main` via the `WD_DEV` resource slot instead of a `steal()` here. The
/// slot holds the built driver, not the `Peri`, because `Watchdog::new` consumes
/// `Peri<'static, WATCHDOG>` (the driver has no lifetime parameter to reborrow
/// into). The task never returns (detached, loops forever), so the shell's restore
/// is moot — the resource is retained for life, exactly the watchdog contract.
pub(crate) async fn watchdog_task(
    node: &'static TaskNode,
    wd: &mut embassy_rp::watchdog::Watchdog,
) {
    node.set_detached(true);
    // Blocked-task detector (feature `trace`). Two complementary checks:
    // - `stalled_task`: an in-flight poll > 100 ms. For a stall on this task's OWN
    //   thread executor it can rarely fire (a blocked executor also blocks the
    //   feeder), but stalls on the other executors (HIGH tier, core 1) are
    //   observable live — see the README's executor table. So additionally:
    // - `max_poll_ticks` watermark: post-hoc, names any node whose longest single
    //   poll exceeded the threshold — works even when observed after the fact.
    //   Warn only on increase to avoid log spam (16 slots cover this graph).
    const STALL_TICKS: u32 = (embassy_time::TICK_HZ / 10) as u32; // 100 ms
    let mut warned = [0u32; 16];
    loop {
        wd.feed(embassy_time::Duration::from_secs(8)); // `feed` also sets the timeout
        for id in embassy_supervisor::trace::executors() {
            if id == 0 {
                continue;
            }
            if let Some((stalled, ticks)) = embassy_supervisor::trace::stalled_task(id, STALL_TICKS)
            {
                defmt::warn!(
                    "trace: {} has been polling for {} ticks",
                    stalled.name,
                    ticks
                );
            }
        }
        for (n, w) in GRAPH.nodes.iter().flatten().zip(warned.iter_mut()) {
            let max = n.max_poll_ticks();
            if max > STALL_TICKS && max > *w {
                *w = max;
                defmt::warn!("trace: {} once held the executor {} ticks", n.name, max);
            }
        }
        embassy_time::Timer::after(embassy_time::Duration::from_secs(2)).await;
    }
}
