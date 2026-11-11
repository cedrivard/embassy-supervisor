//! Fault injection (`fault-inject`): stall, wedge, crash or hog a node on
//! request.

use super::*;

// ─── Fault injection (`fault-inject`) ─────────────────────────────────────

/// A fault injected into a node through [`TaskNode::inject`]. The verbs act on
/// the task, not the worker.
///
/// - **Stall**: stop polling the worker.
/// - **Wedge**: hide shutdown and swallow the ack.
/// - **Crash**: drop the worker future.
/// - **Hog**: busy-spin the executor for the given bound.
///
/// Off by default. For benches, tests, and dashboards.
#[cfg(feature = "fault-inject")]
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// Healthy.
    None,
    /// Stop polling the worker.
    Stall,
    /// Hide the shutdown request and swallow the ack.
    Wedge,
    /// Drop the worker future; the node reads as exited.
    Crash,
    /// Busy-spin the executor for the given bound.
    Hog(embassy_time::Duration),
}

#[cfg(feature = "fault-inject")]
impl Fault {
    /// The verb as a short lowercase name (`"stall"`, `"wedge"`, `"crash"`,
    /// `"hog"`; `None` is `"none"`), for logs and JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Fault::None => "none",
            Fault::Stall => "stall",
            Fault::Wedge => "wedge",
            Fault::Crash => "crash",
            Fault::Hog(_) => "hog",
        }
    }

    pub(crate) fn encode(self) -> (u8, u32) {
        match self {
            Fault::None => (0, 0),
            Fault::Stall => (1, 0),
            Fault::Wedge => (2, 0),
            Fault::Crash => (3, 0),
            Fault::Hog(d) => (4, d.as_millis().min(u32::MAX as u64) as u32),
        }
    }

    pub(crate) fn decode(code: u8, ms: u32) -> Self {
        match code {
            1 => Fault::Stall,
            2 => Fault::Wedge,
            3 => Fault::Crash,
            4 => Fault::Hog(embassy_time::Duration::from_millis(ms as u64)),
            _ => Fault::None,
        }
    }
}

#[cfg(all(feature = "fault-inject", feature = "defmt"))]
impl defmt::Format for Fault {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "{}", self.as_str());
    }
}

/// Why [`TaskNode::inject`] refused.
#[cfg(feature = "fault-inject")]
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InjectError {
    /// The node is a hand-written `spawn:` fn; only wedge applies.
    NoShell,
}

#[cfg(all(feature = "fault-inject", feature = "defmt"))]
impl defmt::Format for InjectError {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(
            fmt,
            "no shell: only a `task:` node can be stalled, crashed or hogged"
        );
    }
}

#[cfg(feature = "fault-inject")]
impl core::fmt::Display for InjectError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InjectError::NoShell => {
                f.write_str("no shell: only a `task:` node can be stalled, crashed or hogged")
            }
        }
    }
}

#[cfg(feature = "fault-inject")]
impl core::error::Error for InjectError {}

/// State for an in-progress `Hog` fault.
#[cfg(feature = "fault-inject")]
struct HogState {
    grace: Timer,
    bound: embassy_time::Duration,
}

/// Wrapper for `task:` worker futures. Returns `Some(output)` on completion,
/// or `None` if [`Fault::Crash`] dropped the worker.
#[cfg(feature = "fault-inject")]
#[doc(hidden)]
pub struct Injected<'a, F> {
    node: &'a TaskNode,
    inner: Option<F>,
    hog: Option<HogState>,
}

#[cfg(feature = "fault-inject")]
impl<'a, F: Future + Unpin> Injected<'a, F> {
    /// Grace period before a `Hog` spin begins.
    pub const HOG_GRACE: embassy_time::Duration = embassy_time::Duration::from_millis(250);

    #[doc(hidden)]
    pub fn new(node: &'a TaskNode, inner: F) -> Self {
        Self {
            node,
            inner: Some(inner),
            hog: None,
        }
    }
}

#[cfg(feature = "fault-inject")]
impl<F: Future + Unpin> Future for Injected<'_, F> {
    type Output = Option<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let node = this.node;
        node.handle.fault_waker.register(cx.waker());
        loop {
            match node.fault() {
                Fault::Stall if !node.handle.flag(flag::SHUTDOWN) => {
                    // A stall ends any armed hog grace: a later Hog starts its own.
                    this.hog = None;
                    if core::pin::pin!(node.handle.shutdown_wake.wait())
                        .poll(cx)
                        .is_ready()
                    {
                        continue;
                    }
                    return Poll::Pending;
                }
                Fault::Crash => {
                    this.inner = None;
                    let _ = node.handle.fault.compare_exchange(
                        3,
                        0,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                    warn!("supervisor: {} crashed (injected)", node.name());
                    return Poll::Ready(None);
                }
                Fault::Hog(bound) => {
                    if !matches!(&this.hog, Some(h) if h.bound == bound) {
                        this.hog = Some(HogState {
                            grace: Timer::after(Injected::<F>::HOG_GRACE),
                            bound,
                        });
                    }
                    let hog = this.hog.as_mut().unwrap();
                    if Pin::new(&mut hog.grace).poll(cx).is_pending() {
                        return Poll::Pending;
                    }
                    let bound = hog.bound;
                    this.hog = None;
                    warn!(
                        "supervisor: {} hogging its executor for {} ms (injected)",
                        node.name(),
                        bound.as_millis()
                    );
                    let deadline = embassy_time::Instant::now() + bound;
                    while embassy_time::Instant::now() < deadline {
                        core::hint::spin_loop();
                    }
                    let _ = node.handle.fault.compare_exchange(
                        4,
                        0,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                }
                _ => this.hog = None,
            }
            break;
        }
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Pending;
        };
        match Pin::new(inner).poll(cx) {
            Poll::Ready(out) => {
                this.inner = None;
                Poll::Ready(Some(out))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
