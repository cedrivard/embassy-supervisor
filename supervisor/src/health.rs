//! Health events: what the liveness monitor reports to the application.

use super::*;

// ─── Health events (supervisor → app) ─────────────────────────────────────
//
// The reporting half of `liveness-monitor`. Deliberately a mailbox rather than
// a callback: the supervisor names what it saw, and the application decides —
// on its own task, at its own priority — what that means. See the module docs
// on why escalation is not built in.

/// What the monitor observed about a node.
///
/// `#[non_exhaustive]`: further observations may be added without a breaking
/// change, so match with a `_` arm.
#[cfg(feature = "liveness-monitor")]
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HealthKind {
    /// The node is still marked running but has not beaten within its
    /// `beat_timeout:` for `beat_window:` consecutive sweeps: alive but
    /// stalled, parked on an await that will never complete. Emitted **once**
    /// per stall; the next event for this node is a [`Recovered`](Self::Recovered),
    /// unless the node stops or is detached first: a stop ends the stall without
    /// an event, and the node's next event after it runs again is a fresh `Stale`.
    Stale {
        /// Ticks since the node's last credited beat, taken when this event was
        /// sent: later than the sweep that tripped when the mailbox was full
        /// (saturating at `u32::MAX / 2`).
        ticks: u32,
    },
    /// A node previously reported [`Stale`](Self::Stale) has beaten again.
    Recovered,
}

/// One observation from [`Supervisor::monitor`], delivered through
/// [`wait_health`].
#[cfg(feature = "liveness-monitor")]
#[derive(Clone, Copy, Debug)]
pub struct HealthEvent {
    /// The node the observation is about.
    pub node: &'static TaskNode,
    /// What was observed.
    pub kind: HealthKind,
}

#[cfg(all(feature = "liveness-monitor", feature = "defmt"))]
impl defmt::Format for HealthKind {
    fn format(&self, fmt: defmt::Formatter) {
        match self {
            HealthKind::Stale { ticks } => defmt::write!(fmt, "stale for {} ticks", ticks),
            HealthKind::Recovered => defmt::write!(fmt, "recovered"),
        }
    }
}

/// Supervisor → app health mailbox. The monitor never blocks on a slow or
/// absent consumer: a transition that finds it full sets the node's
/// `HEALTH_PENDING` flag, and a later sweep sends it again.
#[cfg(feature = "liveness-monitor")]
static HEALTH_EVT: Channel<CriticalSectionRawMutex, HealthEvent, 4> = Channel::new();

/// Await the next health observation from [`Supervisor::monitor`].
///
/// The application's escalation point. What to do with a `Stale` report is
/// domain-specific and therefore yours: log it, degrade to a safe mode, request
/// a `Deactivate`, `clear_ready()` the node so future bring-up defers. The
/// supervisor deliberately does none of these on its own. While a node keeps
/// running its events alternate `Stale`, `Recovered`; a stop ends a stall
/// without an event, so the next one is a fresh `Stale`. A consumer that falls
/// behind gets them late, never twice; a transition reversed before it could be
/// delivered is dropped together with its reversal.
///
/// ```ignore
/// loop {
///     let ev = embassy_supervisor::wait_health().await;
///     match ev.kind {
///         HealthKind::Stale { ticks } => warn!("{} stalled for {} ticks", ev.node.name(), ticks),
///         HealthKind::Recovered => info!("{} is beating", ev.node.name()),
///         _ => {}
///     }
/// }
/// ```
#[cfg(feature = "liveness-monitor")]
pub async fn wait_health() -> HealthEvent {
    HEALTH_EVT.receive().await
}

/// Non-blocking [`wait_health`], for a consumer that polls (a status endpoint
/// draining pending events, an existing loop that must not park here).
#[cfg(feature = "liveness-monitor")]
pub fn try_wait_health() -> Option<HealthEvent> {
    HEALTH_EVT.try_receive().ok()
}

/// Post one observation without blocking: the monitor runs on the supervisor
/// task, which must not park behind a health consumer. A full mailbox leaves
/// the node's `HEALTH_PENDING` flag set for the next sweep, so a consumer that
/// drains late still gets every node's latest transition.
#[cfg(feature = "liveness-monitor")]
fn report_health(node: &'static TaskNode, kind: HealthKind) {
    if HEALTH_EVT.try_send(HealthEvent { node, kind }).is_ok() {
        node.handle.flag_clear(flag::HEALTH_PENDING);
    } else if !node.handle.flag_swap(flag::HEALTH_PENDING, true) {
        warn!(
            "supervisor: health mailbox full, {} is re-reported next sweep",
            node.name()
        );
    }
}

/// One node's share of a monitor sweep: the stop reset, the observed-write
/// poll (`observed`: the graph declares an `observed` entry) and the strike
/// count. Not generic, so every graph's monitor shares one copy.
#[cfg(feature = "liveness-monitor")]
pub(crate) fn sweep_node(node: &'static TaskNode, observed: bool) {
    #[cfg(not(feature = "coupling-observe"))]
    let _ = observed;
    let Some(budget) = node.beat_timeout() else {
        return;
    };
    if node.is_detached() || !node.is_running() {
        // A stop ends a stall without an event; see `HealthKind::Stale`.
        // Both are usually clear already: test before writing.
        if node.handle.stale_strikes.load(Ordering::Relaxed) != 0 {
            node.handle.stale_strikes.store(0, Ordering::Release);
        }
        if node.handle.flag(flag::HEALTH_PENDING) {
            node.handle.flag_clear(flag::HEALTH_PENDING);
        }
        return;
    }

    #[cfg(feature = "coupling-observe")]
    if observed && node.poll_observed_writes() {
        node.beat();
        #[cfg(feature = "readiness")]
        if node.ready_on_write() && !node.is_ready() {
            node.set_ready();
        }
    }

    let window = node.beat_window().max(1);
    let pending = node.handle.flag(flag::HEALTH_PENDING);
    if node.is_stale(budget) {
        let strikes = node.handle.stale_strikes.load(Ordering::Acquire);
        if strikes < window {
            let strikes = strikes + 1;
            node.handle.stale_strikes.store(strikes, Ordering::Release);
            if strikes == window && pending {
                // A new stall cancels the undelivered Recovered: the
                // last event the app got for this node is a Stale.
                node.handle.flag_clear(flag::HEALTH_PENDING);
            } else if strikes == window {
                warn!(
                    "supervisor: {} has not beaten in {} ticks",
                    node.name(),
                    node.ticks_since_beat()
                );
                report_health(
                    node,
                    HealthKind::Stale {
                        ticks: node.ticks_since_beat(),
                    },
                );
            } else if pending {
                report_health(node, HealthKind::Recovered);
            }
        } else if pending {
            report_health(
                node,
                HealthKind::Stale {
                    ticks: node.ticks_since_beat(),
                },
            );
        }
    } else {
        let had = node.handle.stale_strikes.swap(0, Ordering::AcqRel);
        if had >= window && pending {
            // The stall ended before its Stale was delivered.
            node.handle.flag_clear(flag::HEALTH_PENDING);
        } else if had >= window {
            info!("supervisor: {} is beating", node.name());
            report_health(node, HealthKind::Recovered);
        } else if pending {
            report_health(node, HealthKind::Recovered);
        }
    }
}
