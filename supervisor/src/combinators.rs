//! The futures behind `run_cancellable` and `run_pausable`, and their
//! results.

use super::*;

/// The shutdown side of [`TaskNode::run_cancellable`]'s result: the raced work
/// future was cancelled at its await point because a stop/pause request won the
/// select. Pairs naturally with the `exit:` slot — a worker returning
/// `Result<R, Aborted>` records completed-vs-cancelled for whoever reads the
/// exit value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Aborted;

#[cfg(feature = "defmt")]
impl defmt::Format for Aborted {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "aborted by shutdown");
    }
}

impl core::fmt::Display for Aborted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("aborted by shutdown")
    }
}

impl core::error::Error for Aborted {}

/// The pause side of [`TaskNode::run_pausable`]'s result: a stop/pause request
/// won the race, the combinator acked and parked, and the supervisor has since
/// resumed the node. By the time a body sees `Err(Resumed)` the park is already
/// over — the next loop iteration is the fresh cycle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resumed;

#[cfg(feature = "defmt")]
impl defmt::Format for Resumed {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "resumed after pause");
    }
}

impl core::fmt::Display for Resumed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("resumed after pause")
    }
}

impl core::error::Error for Resumed {}

pin_project_lite::pin_project! {
    /// The future behind [`TaskNode::run_cancellable`] and
    /// [`run_cancellable_acked`](TaskNode::run_cancellable_acked): races the
    /// worker against the node's shutdown signal, holding the worker's state
    /// machine **once**.
    ///
    /// Written by hand rather than as `select(fut, wait_shutdown()).await` inside
    /// an `async fn`, because that shape stores the worker both as the function's
    /// argument and inside the select (rust-lang/rust#62958) — a doubling paid in
    /// every caller's static task storage, which for a graph of `cancel` nodes is
    /// the sum of every worker future in the binary.
    ///
    /// `fut` is an `Option` so the abort path can drop the worker in place, via
    /// the safe `Pin::set`, *before* acking: the ack is what releases the
    /// supervisor's teardown wait, and a runner whose `Drop` frees hardware must
    /// have run by then.
    ///
    /// Shutdown is polled inline off `node` rather than stored as a
    /// `wait_shutdown()` future: the signal wait is stateless (register-on-poll),
    /// so embedding its state machine here would spend ~8 bytes of every
    /// caller's task storage carrying a second copy of the node reference.
    pub(crate) struct RunCancellable<'a, F> {
        #[pin]
        pub(crate) fut: Option<F>,
        pub(crate) node: &'a TaskNode,
        // True only for the `_acked` variant.
        pub(crate) ack: bool,
    }
}

impl<F: Future> Future for RunCancellable<'_, F> {
    type Output = Result<F::Output, Aborted>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        // Worker first, shutdown second — the polling order `select` gave this
        // before, so a worker that completes in the same wake as a stop request
        // still reports completion rather than abort.
        if let Some(fut) = this.fut.as_mut().as_pin_mut()
            && let Poll::Ready(out) = fut.poll(cx)
        {
            return Poll::Ready(Ok(out));
        }
        // The flag fast path covers a request that predates this future (the
        // signal is edge-triggered); a fresh `wait()` per poll is sound because
        // the `Signal` latches its value and re-registers the waker each poll.
        if this.node.poll_shutdown(cx) {
            this.fut.set(None);
            if *this.ack {
                this.node.ack_dropped();
            }
            return Poll::Ready(Err(Aborted));
        }
        Poll::Pending
    }
}

pin_project_lite::pin_project! {
    /// The future behind [`TaskNode::run_pausable`]: [`RunCancellable`] with the
    /// `Pause` protocol's tail folded in. Racing, it behaves as the `_acked`
    /// variant; on abort it drops the worker in place, acks, and becomes the
    /// `wait_resume()` park — one state flag, and the worker's state machine is
    /// still held exactly once (same layout rationale as [`RunCancellable`]).
    pub(crate) struct RunPausable<'a, F> {
        #[pin]
        pub(crate) fut: Option<F>,
        pub(crate) node: &'a TaskNode,
        pub(crate) parked: bool,
    }
}

impl<F: Future> Future for RunPausable<'_, F> {
    type Output = Result<F::Output, Resumed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        if !*this.parked {
            // Same race, same ordering as `RunCancellable`: worker first, so a
            // completion in the same wake as a stop request still reports
            // completion; flag fast path, then the latched signal.
            if let Some(fut) = this.fut.as_mut().as_pin_mut()
                && let Poll::Ready(out) = fut.poll(cx)
            {
                return Poll::Ready(Ok(out));
            }
            if !this.node.poll_shutdown(cx) {
                return Poll::Pending;
            }
            this.fut.set(None);
            this.node.ack_dropped();
            *this.parked = true;
            if !matches!(this.node.mode(), Mode::Pause) {
                warn!(
                    "supervisor: {} parked through run_pausable but its mode is {}: nothing resumes it",
                    this.node.name(),
                    this.node.mode()
                );
            }
            // Fall through to the resume poll in this same call: `resume_node`
            // is eligible the instant the ack lands, and a resume signaled
            // before this poll returns would otherwise latch with no waker
            // registered — a lost wakeup.
        }
        if core::pin::pin!(this.node.handle.resume_wake.wait())
            .poll(cx)
            .is_ready()
        {
            return Poll::Ready(Err(Resumed));
        }
        Poll::Pending
    }
}
