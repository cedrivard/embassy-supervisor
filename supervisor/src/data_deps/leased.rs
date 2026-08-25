use core::ops::Deref;
use core::sync::atomic::Ordering;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use portable_atomic::AtomicU32;

use crate::{Sig, TaskNode};

const CLOSED: u32 = 1 << 31;
const COUNT: u32 = !CLOSED;

#[repr(C)]
/// A signal-like value whose producer can wait until all outstanding readers finish.
///
/// `Leased` counts live claims; [`drain`](Self::drain) blocks until the count
/// reaches zero, which is used to hold a producer's stop until consumers are done.
pub struct Leased<T> {
    inner: T,
    state: AtomicU32,
    idle: Signal<CriticalSectionRawMutex, ()>,
}

impl<T> Leased<T> {
    /// Wrap `inner` as a leased value.
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            state: AtomicU32::new(0),
            idle: Signal::new(),
        }
    }

    /// Return the number of live leases.
    pub fn leases(&self) -> u32 {
        self.state.load(Ordering::Acquire) & COUNT
    }

    /// Return `true` if the value has been drained.
    pub fn is_drained(&self) -> bool {
        self.state.load(Ordering::Acquire) & CLOSED != 0
    }

    /// Close the value to new leases and wait until all current leases drop.
    pub async fn drain(&self) {
        self.state.fetch_or(CLOSED, Ordering::AcqRel);
        while self.state.load(Ordering::Acquire) & COUNT != 0 {
            self.idle.wait().await;
        }
    }

    /// Reopen the value to new leases after a previous drain.
    pub fn reopen(&self) {
        self.idle.reset();
        self.state.fetch_and(!CLOSED, Ordering::AcqRel);
    }

    /// Acquire a live lease, or `None` if the value is drained.
    pub fn lease(&'static self) -> Option<Lease<T>> {
        let mut cur = self.state.load(Ordering::Acquire);
        loop {
            if cur & CLOSED != 0 {
                return None;
            }
            match self.state.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Lease { target: self }),
                Err(seen) => cur = seen,
            }
        }
    }
}

/// The wrapper is transparent to everything that is not a lease.
impl<T> Deref for Leased<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

/// Polling a wrapped signal is polling what it wraps, so an `observed` entry
/// keeps working when a signal gains a lease count.
#[cfg(feature = "coupling-observe")]
impl<T: crate::Observable> crate::Observable for Leased<T> {
    fn change_token(&self) -> u32 {
        self.inner.change_token()
    }
}

/// A live claim on a [`Leased`] signal: the producer's `drain` does not return
/// until every `Lease` has been dropped.
pub struct Lease<T: 'static> {
    target: &'static Leased<T>,
}

impl<T> Deref for Lease<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.target.inner
    }
}

impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        if self.target.state.fetch_sub(1, Ordering::AcqRel) & COUNT == 1 {
            self.target.idle.signal(());
        }
    }
}

impl TaskNode {
    /// Acquire a lease on a `Sig<Leased<T>>` signal.
    pub fn lease<T: Sync>(&self, s: Sig<Leased<T>>) -> Option<Lease<T>> {
        s.target.lease()
    }
}
