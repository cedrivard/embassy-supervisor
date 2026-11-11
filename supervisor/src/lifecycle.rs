//! Lifecycle events, the lifecycle sink and the bring-up breadcrumb.

use super::*;

// ─── Lifecycle events & breadcrumb ──────────────────────────────────────

/// What happened to a node, in a [`LifecycleEvent`].
///
/// `#[non_exhaustive]`: new transitions may be added without a breaking
/// change.
#[cfg(any(feature = "lifecycle-events", feature = "lifecycle-sink"))]
#[non_exhaustive]
#[derive(Clone, Copy, Debug)]
pub enum LifecycleKind {
    /// A fresh instance was spawned — or, for a node the application spawns
    /// itself (no `spawn`), marked running.
    Started,
    /// A parked `Pause` instance was resumed in place.
    Resumed,
    /// The supervisor signalled the node to stop.
    StopRequested,
    /// The node acknowledged a stop without its body returning: a `Pause`
    /// instance parked on `wait_resume()`, or a hand-written `ack_dropped()`.
    Stopped,
    /// The node acknowledged a stop by its body returning
    /// ([`TaskNode::mark_exited`], which every `task:` shell calls): the
    /// instance is gone. Only observed on a supervisor stop path — a body that
    /// returns on its own, unasked, is not evented (the supervisor is not
    /// watching it; [`TaskNode::has_exited`] reads it).
    Exited,
    /// A lifecycle step failed for this node: the same [`FaultKind`] the verb
    /// returns (or, on a best-effort wave, logs).
    Faulted(FaultKind),
}

#[cfg(any(feature = "lifecycle-events", feature = "lifecycle-sink"))]
impl LifecycleKind {
    /// Stable lower-case wire name (`"started"`, `"resumed"`,
    /// `"stop_requested"`, `"stopped"`, `"exited"`, `"faulted"`), for logs and
    /// JSON.
    pub const fn as_str(&self) -> &'static str {
        match self {
            LifecycleKind::Started => "started",
            LifecycleKind::Resumed => "resumed",
            LifecycleKind::StopRequested => "stop_requested",
            LifecycleKind::Stopped => "stopped",
            LifecycleKind::Exited => "exited",
            LifecycleKind::Faulted(_) => "faulted",
        }
    }
}

#[cfg(all(
    any(feature = "lifecycle-events", feature = "lifecycle-sink"),
    feature = "defmt"
))]
impl defmt::Format for LifecycleKind {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "{}", self.as_str());
    }
}

/// One lifecycle transition, delivered through `wait_lifecycle`
/// (`lifecycle-events`) or the `set_lifecycle_sink` callback (`lifecycle-sink`).
#[cfg(any(feature = "lifecycle-events", feature = "lifecycle-sink"))]
#[derive(Clone, Copy, Debug)]
pub struct LifecycleEvent {
    /// The node the transition is about.
    pub node: &'static TaskNode,
    /// What happened.
    pub kind: LifecycleKind,
}

/// Supervisor → app lifecycle queue. Lossy on purpose: the waves run on the
/// supervisor task, which must never park behind a slow or absent consumer.
/// One consumer.
#[cfg(feature = "lifecycle-events")]
static LIFECYCLE_EVT: Channel<CriticalSectionRawMutex, LifecycleEvent, 8> = Channel::new();

/// Events [`emit_lifecycle`] dropped on a full queue, wrapping.
#[cfg(feature = "lifecycle-events")]
static LIFECYCLE_DROPPED: AtomicU32 = AtomicU32::new(0);

/// Await the next lifecycle transition.
///
/// Every transition the supervisor drives — waves, single-node verbs, pool
/// scaling, bound-dep cascades — posts one event. The queue holds 8; while it
/// is full further events are dropped and counted
/// ([`lifecycle_events_dropped`]), so a consumer started after a large
/// bring-up sees the head of it, not the tail.
///
/// ```ignore
/// loop {
///     let ev = embassy_supervisor::wait_lifecycle().await;
///     info!("{} {}", ev.node.name(), ev.kind.as_str());
/// }
/// ```
#[cfg(feature = "lifecycle-events")]
pub async fn wait_lifecycle() -> LifecycleEvent {
    LIFECYCLE_EVT.receive().await
}

/// Non-blocking [`wait_lifecycle`], for a consumer that polls.
#[cfg(feature = "lifecycle-events")]
pub fn try_wait_lifecycle() -> Option<LifecycleEvent> {
    LIFECYCLE_EVT.try_receive().ok()
}

/// Total lifecycle events dropped because the queue was full (wrapping).
/// Sample it beside the events to know whether the sequence has holes.
#[cfg(feature = "lifecycle-events")]
pub fn lifecycle_events_dropped() -> u32 {
    LIFECYCLE_DROPPED.load(Ordering::Relaxed)
}

/// A lifecycle callback; see [`set_lifecycle_sink`].
#[cfg(feature = "lifecycle-sink")]
pub type LifecycleSink = fn(LifecycleEvent);

/// The callback [`set_lifecycle_sink`] installed.
#[cfg(feature = "lifecycle-sink")]
static LIFECYCLE_SINK: BlockingMutex<CriticalSectionRawMutex, Cell<Option<LifecycleSink>>> =
    BlockingMutex::new(Cell::new(None));

/// Have every lifecycle transition call `sink`, synchronously, at the moment
/// the supervisor drives it. Replaces any previous sink.
///
/// The push-side alternative to the `wait_lifecycle` queue
/// (`lifecycle-events`), for an app that only records or logs events: no
/// queue, no consumer task, nothing dropped, and a timestamp taken inside
/// `sink` is the transition's own. The price is the calling context: `sink`
/// runs on the supervisor task, inside the wave or verb that made the
/// transition — on whatever executor (an interrupt executor, another core)
/// that task runs on. Keep it short and non-blocking: a ring-buffer write or
/// a log line, never a lock another task may hold for long. Events before the
/// first call are not delivered. With `lifecycle-events` on too, every event
/// reaches both.
///
/// ```ignore
/// fn record(ev: LifecycleEvent) {
///     info!("{} {}", ev.node.name(), ev.kind.as_str());
/// }
/// embassy_supervisor::set_lifecycle_sink(record);
/// ```
#[cfg(feature = "lifecycle-sink")]
pub fn set_lifecycle_sink(sink: LifecycleSink) {
    LIFECYCLE_SINK.lock(|c| c.set(Some(sink)));
}

/// Deliver one event to whichever observers are compiled in. Never blocks:
/// the queue counts a full slot as dropped.
#[cfg(any(feature = "lifecycle-events", feature = "lifecycle-sink"))]
pub(crate) fn emit_lifecycle(ev: LifecycleEvent) {
    #[cfg(feature = "lifecycle-sink")]
    if let Some(sink) = LIFECYCLE_SINK.lock(|c| c.get()) {
        sink(ev);
    }
    #[cfg(feature = "lifecycle-events")]
    if LIFECYCLE_EVT.try_send(ev).is_err() {
        LIFECYCLE_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// The last lifecycle phase a node passed through ([`TaskNode::crumb`]), and
/// the phase field of [`crumb_word`].
///
/// `#[non_exhaustive]`: further phases may be added without a breaking
/// change, so match with a `_` arm.
#[cfg(feature = "breadcrumb")]
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CrumbPhase {
    /// Never touched by a supervisor since boot.
    Idle = 0,
    /// Picked for bring-up: gates (executor slot, resources, ready deps) are
    /// being waited on, or the spawn is in progress.
    Spawning = 1,
    /// Spawned (or, without a `spawn`, marked running).
    Started = 2,
    /// Signalled to stop; the ack has not arrived.
    StopSignalled = 3,
    /// Acknowledged its stop (parked or exited).
    Stopped = 4,
    /// A lifecycle step faulted (gate timeout, spawn failure, missed ack).
    Faulted = 5,
    /// A parked `Pause` instance was resumed.
    Resumed = 6,
}

#[cfg(feature = "breadcrumb")]
impl CrumbPhase {
    pub(crate) const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => CrumbPhase::Idle,
            1 => CrumbPhase::Spawning,
            2 => CrumbPhase::Started,
            3 => CrumbPhase::StopSignalled,
            4 => CrumbPhase::Stopped,
            5 => CrumbPhase::Faulted,
            6 => CrumbPhase::Resumed,
            _ => return None,
        })
    }

    /// Stable lower-case wire name, for logs and JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            CrumbPhase::Idle => "idle",
            CrumbPhase::Spawning => "spawning",
            CrumbPhase::Started => "started",
            CrumbPhase::StopSignalled => "stop_signalled",
            CrumbPhase::Stopped => "stopped",
            CrumbPhase::Faulted => "faulted",
            CrumbPhase::Resumed => "resumed",
        }
    }
}

#[cfg(all(feature = "breadcrumb", feature = "defmt"))]
impl defmt::Format for CrumbPhase {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "{}", self.as_str());
    }
}

/// The binary-wide last crumb: `0x8000_0000 | phase << 16 | slot`. 0 = none.
#[cfg(feature = "breadcrumb")]
static LAST_CRUMB: AtomicU32 = AtomicU32::new(0);

/// Slot field of a crumb word whose node is not in the driving graph.
#[cfg(feature = "breadcrumb")]
const CRUMB_NO_SLOT: u16 = u16::MAX;

/// The most recent lifecycle transition any supervisor drove, as one word:
/// bit 31 = valid, bits 16..24 = [`CrumbPhase`], bits 0..16 = the node's slot
/// index in its graph (`GRAPH.nodes[i]`). 0 until the first transition.
///
/// Fixed-size on purpose: stash it in no-init RAM or a watchdog scratch
/// register from the feeder or the panic handler, and [`decode_crumb`] it on
/// the next boot to name the node and phase a reset interrupted (a bring-up
/// that hung in `Spawning`, a teardown stuck in `StopSignalled`).
///
/// Last writer wins across graphs and concurrent waves, and a multi-graph
/// binary cannot tell from the word which graph's slot it is. The per-node
/// [`TaskNode::crumb`] is the authoritative dump:
/// `GRAPH.nodes.iter().flatten().map(|n| (n.name(), n.crumb()))`.
#[cfg(feature = "breadcrumb")]
pub fn crumb_word() -> u32 {
    LAST_CRUMB.load(Ordering::Relaxed)
}

/// Split a [`crumb_word`] into its phase and slot index; `None` for 0 (no
/// transition yet) or a word that is not a crumb. A slot of `u16::MAX` means
/// the node was driven through a supervisor whose graph does not contain it.
#[cfg(feature = "breadcrumb")]
pub const fn decode_crumb(word: u32) -> Option<(CrumbPhase, u16)> {
    if word & 0x8000_0000 == 0 || word & 0x7F00_0000 != 0 {
        return None;
    }
    match CrumbPhase::from_u8((word >> 16) as u8) {
        Some(phase) => Some((phase, word as u16)),
        None => None,
    }
}

/// One supervisor-driven transition, fanned out to whichever observers are
/// compiled in (lifecycle events, breadcrumb). With neither, `note` is empty.
// The fault payload is read only by the event queue.
#[cfg_attr(
    not(any(feature = "lifecycle-events", feature = "lifecycle-sink")),
    allow(dead_code)
)]
#[derive(Clone, Copy)]
pub(crate) enum Transition {
    Spawning,
    Started,
    Resumed,
    StopRequested,
    Stopped,
    Exited,
    Faulted(FaultKind),
}

/// Record `t` for `node` at graph slot `slot` (`None`: not in this graph).
#[inline(always)]
pub(crate) fn note(node: &'static TaskNode, slot: Option<usize>, t: Transition) {
    #[cfg(feature = "breadcrumb")]
    {
        let phase = match t {
            Transition::Spawning => CrumbPhase::Spawning,
            Transition::Started => CrumbPhase::Started,
            Transition::Resumed => CrumbPhase::Resumed,
            Transition::StopRequested => CrumbPhase::StopSignalled,
            Transition::Stopped | Transition::Exited => CrumbPhase::Stopped,
            Transition::Faulted(_) => CrumbPhase::Faulted,
        };
        node.handle.crumb.store(phase as u8, Ordering::Relaxed);
        let slot = slot.map_or(CRUMB_NO_SLOT, |i| i as u16);
        LAST_CRUMB.store(
            0x8000_0000 | (phase as u32) << 16 | u32::from(slot),
            Ordering::Relaxed,
        );
    }
    #[cfg(any(feature = "lifecycle-events", feature = "lifecycle-sink"))]
    {
        let kind = match t {
            Transition::Spawning => None,
            Transition::Started => Some(LifecycleKind::Started),
            Transition::Resumed => Some(LifecycleKind::Resumed),
            Transition::StopRequested => Some(LifecycleKind::StopRequested),
            Transition::Stopped => Some(LifecycleKind::Stopped),
            Transition::Exited => Some(LifecycleKind::Exited),
            Transition::Faulted(k) => Some(LifecycleKind::Faulted(k)),
        };
        if let Some(kind) = kind {
            emit_lifecycle(LifecycleEvent { node, kind });
        }
    }
    #[cfg(not(feature = "breadcrumb"))]
    let _ = slot;
    #[cfg(not(any(
        feature = "lifecycle-events",
        feature = "lifecycle-sink",
        feature = "breadcrumb"
    )))]
    let _ = (node, t);
}

/// The resolve-side transition for an acked node: its body returned, or it
/// only acknowledged (parked / hand-written ack).
pub(crate) fn acked(node: &TaskNode) -> Transition {
    if node.has_exited() {
        Transition::Exited
    } else {
        Transition::Stopped
    }
}
