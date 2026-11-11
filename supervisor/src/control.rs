//! The control mailbox: application requests to activate, deactivate or
//! restart a node, drained by the supervisor's driver loop.

use super::*;

#[cfg(feature = "control")]
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// A control request issued to the supervisor for a node.
pub enum ControlOp {
    /// Request that the node be activated.
    Activate,
    /// Request that the node be deactivated.
    Deactivate,
    /// Request that the node be restarted.
    #[cfg(feature = "restart")]
    Restart,
}

#[cfg(feature = "control")]
impl ControlOp {
    /// Stable lower-case name, used for `defmt` and [`Display`](core::fmt::Display)
    /// alike.
    pub const fn as_str(&self) -> &'static str {
        match self {
            ControlOp::Activate => "activate",
            ControlOp::Deactivate => "deactivate",
            #[cfg(feature = "restart")]
            ControlOp::Restart => "restart",
        }
    }
}

#[cfg(all(feature = "control", feature = "defmt"))]
impl defmt::Format for ControlOp {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "{}", self.as_str());
    }
}

#[cfg(feature = "control")]
impl core::fmt::Display for ControlOp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(feature = "control")]
#[derive(Clone, Copy, Debug)]
/// A control request addressed to a specific node.
pub struct ControlCommand {
    /// The target node.
    pub node: &'static TaskNode,
    /// The direction to drive it.
    pub op: ControlOp,
}

/// App → supervisor control mailbox. `&'static TaskNode` is `Copy + Sync`, so
/// the target rides the channel directly — no name lookup needed supervisor-side.
#[cfg(feature = "control")]
pub(crate) static CONTROL_REQ: Channel<CriticalSectionRawMutex, ControlCommand, 4> = Channel::new();

/// The control mailbox was full (4 outstanding requests) and the request was
/// not enqueued. Returned by [`try_request_control`]; retry after the
/// supervisor's driver loop has drained a command, or use the awaiting
/// [`request_control`] from async contexts.
#[cfg(feature = "control")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ControlQueueFull;

#[cfg(all(feature = "control", feature = "defmt"))]
impl defmt::Format for ControlQueueFull {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "control queue full");
    }
}

#[cfg(feature = "control")]
impl core::fmt::Display for ControlQueueFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("control queue full")
    }
}

#[cfg(feature = "control")]
impl core::error::Error for ControlQueueFull {}

/// Enqueue a control request, waiting for mailbox capacity if it is full.
/// Lossless — the request is delivered once the supervisor's driver loop drains
/// an earlier command. Called by the application's control surface. One task
/// at a time may wait here when the mailbox is full.
#[cfg(feature = "control")]
pub async fn request_control(node: &'static TaskNode, op: ControlOp) {
    CONTROL_REQ.send(ControlCommand { node, op }).await;
}

/// Non-blocking variant of [`request_control`] for sync contexts (ISRs,
/// callbacks). Fails with [`ControlQueueFull`] instead of dropping the request
/// when the mailbox is full — the caller decides whether to retry or surface it.
#[cfg(feature = "control")]
pub fn try_request_control(node: &'static TaskNode, op: ControlOp) -> Result<(), ControlQueueFull> {
    CONTROL_REQ
        .try_send(ControlCommand { node, op })
        .map_err(|_| ControlQueueFull)
}

/// Await the next control request. Selected by the supervisor's driver loop
/// against pool scaling and any other application wake sources.
#[cfg(feature = "control")]
pub async fn wait_control() -> ControlCommand {
    CONTROL_REQ.receive().await
}
