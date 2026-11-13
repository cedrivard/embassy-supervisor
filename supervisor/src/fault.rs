//! Node-scoped faults: what bring-up, teardown and the control verbs return.

use super::*;

/// A node-scoped lifecycle failure: which node, and what went wrong.
///
/// Every way bring-up or teardown can fail is about one node, so one type says
/// so — the same `{ node, kind }` shape as [`HealthEvent`].
/// It is what [`Supervisor::start`], [`Supervisor::teardown`] and
/// [`Supervisor::run`] return, as well as `Supervisor::activate` (`control`)
/// and `Supervisor::restart` (`restart`) when those features are enabled.
///
/// [`Display`](core::fmt::Display) is unconditional, so an application logging
/// through anything other than `defmt` can render one without matching on
/// [`FaultKind`].
#[derive(Clone, Copy, Debug)]
pub struct NodeFault {
    /// The node the failure is about.
    pub node: &'static TaskNode,
    /// What went wrong.
    pub kind: FaultKind,
}

/// What went wrong in a [`NodeFault`].
///
/// `#[non_exhaustive]`: new failure modes may be added without a breaking
/// change.
#[non_exhaustive]
#[derive(Clone, Copy)]
pub enum FaultKind {
    /// A `ready`-marked dep did not assert readiness within the dependent's
    /// `slot_timeout`. Either the dep is failing to reach its serving state, or
    /// the budget is too tight for this build. Only plain `ready` edges fault
    /// here: a `bound` edge parks the dependent instead.
    ReadyDepTimeout {
        /// The dep that never asserted.
        dep: &'static TaskNode,
    },
    /// A `resources:` slot was still empty at the deadline — nothing called
    /// `provide()` before the supervisor started, or a previous instance never
    /// restored it.
    ResourceMissing,
    /// The node names an `executor:` slot that was never filled with a spawner.
    ExecutorSlotEmpty,
    /// The spawn was rejected with `SpawnError::Busy`: embassy's own case, an
    /// exhausted task pool, or one the supervisor reports the same way. Those are a
    /// resident shell or a previous instance still finishing at the deadline, the
    /// spawn glue's fail-closed check finding a resource or executor slot empty, and
    /// (with `heap-state`) a refused `state:` allocation.
    Spawn(SpawnError),
    /// The node did not acknowledge a requested shutdown within its ack window.
    /// It is still marked running; the sane escalations are app-level. Its
    /// `divisible` shares, if any, have been released on its behalf.
    ShutdownTimeout,
    /// [`Supervisor::restart`] was refused: the node already used its
    /// `restart_budget:` inside the current window. Nothing was stopped; the
    /// node keeps running as it was. A crash loop the app has to see, which is
    /// why `apply_control`'s Restart arm propagates it.
    #[cfg(feature = "restart-intensity")]
    RestartStorm,
}

/// Variant names only: `Spawn` shows the error kind without embassy's
/// `pool_size` hint, and `ReadyDepTimeout` names the dep.
impl core::fmt::Debug for FaultKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ReadyDepTimeout { dep } => f
                .debug_struct("ReadyDepTimeout")
                .field("dep", &dep.name())
                .finish(),
            Self::ResourceMissing => f.write_str("ResourceMissing"),
            Self::ExecutorSlotEmpty => f.write_str("ExecutorSlotEmpty"),
            Self::Spawn(SpawnError::Busy) => f.write_str("Spawn(Busy)"),
            Self::ShutdownTimeout => f.write_str("ShutdownTimeout"),
            #[cfg(feature = "restart-intensity")]
            Self::RestartStorm => f.write_str("RestartStorm"),
        }
    }
}

impl FaultKind {
    /// A bring-up fault: the node did not start, and nothing else changed.
    /// `ReadyDepTimeout`, `ResourceMissing`, `ExecutorSlotEmpty` and `Spawn`
    /// are bring-up faults. `ShutdownTimeout` and `RestartStorm` are not: they
    /// are about a node that is still running. `Supervisor::run` logs bring-up
    /// faults and keeps driving.
    pub const fn is_bring_up(&self) -> bool {
        match self {
            Self::ReadyDepTimeout { .. }
            | Self::ResourceMissing
            | Self::ExecutorSlotEmpty
            | Self::Spawn(_) => true,
            Self::ShutdownTimeout => false,
            #[cfg(feature = "restart-intensity")]
            Self::RestartStorm => false,
        }
    }
}

impl core::fmt::Display for NodeFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.kind {
            FaultKind::ReadyDepTimeout { dep } => write!(
                f,
                "{}: ready-dep {} did not assert within {}ms",
                self.node.name(),
                dep.name(),
                self.node.cfg.slot_timeout_ms
            ),
            FaultKind::ResourceMissing => {
                write!(
                    f,
                    "{}: a resource slot was never provided",
                    self.node.name()
                )
            }
            FaultKind::ExecutorSlotEmpty => {
                write!(
                    f,
                    "{}: its executor slot was never filled",
                    self.node.name()
                )
            }
            FaultKind::Spawn(_) => {
                write!(
                    f,
                    "{}: spawn rejected (pool full, previous instance still finishing, or a slot or state: allocation unavailable)",
                    self.node.name()
                )
            }
            FaultKind::ShutdownTimeout => {
                write!(f, "{}: missed its shutdown ack", self.node.name())
            }
            #[cfg(feature = "restart-intensity")]
            FaultKind::RestartStorm => {
                write!(f, "{}: restart budget exhausted", self.node.name())
            }
        }
    }
}

#[cfg(feature = "defmt")]
impl defmt::Format for NodeFault {
    fn format(&self, fmt: defmt::Formatter) {
        match self.kind {
            FaultKind::ReadyDepTimeout { dep } => defmt::write!(
                fmt,
                "{}: ready-dep {} did not assert within {}ms",
                self.node.name(),
                dep.name(),
                self.node.cfg.slot_timeout_ms
            ),
            FaultKind::ResourceMissing => {
                defmt::write!(
                    fmt,
                    "{}: a resource slot was never provided",
                    self.node.name()
                )
            }
            FaultKind::ExecutorSlotEmpty => {
                defmt::write!(
                    fmt,
                    "{}: its executor slot was never filled",
                    self.node.name()
                )
            }
            FaultKind::Spawn(_) => {
                defmt::write!(
                    fmt,
                    "{}: spawn rejected (pool full, previous instance still finishing, or a slot or state: allocation unavailable)",
                    self.node.name()
                )
            }
            FaultKind::ShutdownTimeout => {
                defmt::write!(fmt, "{}: missed its shutdown ack", self.node.name())
            }
            #[cfg(feature = "restart-intensity")]
            FaultKind::RestartStorm => {
                defmt::write!(fmt, "{}: restart budget exhausted", self.node.name())
            }
        }
    }
}

impl core::error::Error for NodeFault {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.kind {
            FaultKind::Spawn(e) => Some(e),
            _ => None,
        }
    }
}
