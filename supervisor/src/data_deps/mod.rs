#[cfg(feature = "readiness")]
mod backed;
mod gated;
mod leased;

#[cfg(feature = "readiness")]
pub(crate) use backed::notify_serving;
#[cfg(feature = "readiness")]
pub use backed::{Backed, Open};
pub use gated::Gated;
pub use leased::{Lease, Leased};

use crate::{Coupling, TaskNode};

/// Find the node that writes to `entry`, if there is exactly one.
///
/// If multiple writers exist, the first is returned and a warning is logged.
pub fn producer_of(
    caller: &'static TaskNode,
    entry: &'static Coupling,
) -> Option<&'static TaskNode> {
    let mut found: Option<&'static TaskNode> = None;
    let mut count = 0usize;
    for node in caller.graph().iter().flatten() {
        if node.has_entry(entry, true) {
            count += 1;
            let _ = found.get_or_insert(node);
        }
    }
    if count > 1 {
        warn!(
            "supervisor: {} has {} writers; gating it waits on the first",
            entry.name(),
            count
        );
    }
    found
}
