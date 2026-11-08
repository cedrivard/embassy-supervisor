//! A broadcast wake for waits that several tasks share: the wave events and
//! the gated-read serving event.

use core::cell::RefCell;
use core::future::poll_fn;
use core::task::{Poll, Waker};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use portable_atomic::{AtomicU32, Ordering};

/// A generation counter plus up to `N` wakers, all woken by every
/// [`signal`](Self::signal). A `Signal` keeps one waker and hands each signal
/// to one poller; here every parked waiter sees every signal.
///
/// Wakers are deduplicated with `will_wake`, so a task takes one slot however
/// many of its futures wait. A waiter that finds the set full is not
/// registered and nobody is woken to make room: every caller races its wait
/// against a timer of its own, which then stands in for the wake. A waker
/// stays registered until the next `signal`, so a waiter that already left on
/// its timer is woken once more, spuriously. `8N + 8` bytes on a 32-bit target.
pub(crate) struct WakerSet<const N: usize> {
    wakers: Mutex<CriticalSectionRawMutex, RefCell<[Option<Waker>; N]>>,
    /// Bumped by every `signal`. A waiter reads it before testing its
    /// condition and waits for it to move, so a signal landing between the
    /// test and the park is never missed. Written only under the `wakers` lock.
    generation: AtomicU32,
}

impl<const N: usize> WakerSet<N> {
    pub(crate) const fn new() -> Self {
        Self {
            wakers: Mutex::new(RefCell::new([const { None }; N])),
            generation: AtomicU32::new(0),
        }
    }

    /// Register `w` for the next [`signal`](Self::signal). `false` when the
    /// set is full: the caller's own timer has to cover this wait.
    pub(crate) fn register(&self, w: &Waker) -> bool {
        self.wakers.lock(|cell| {
            let mut slots = cell.borrow_mut();
            if slots.iter().flatten().any(|s| s.will_wake(w)) {
                return true;
            }
            match slots.iter_mut().find(|s| s.is_none()) {
                Some(free) => {
                    *free = Some(w.clone());
                    true
                }
                None => false,
            }
        })
    }

    /// Bump the generation and wake every registered waiter. The wakers run
    /// after the critical section, so interrupt latency does not grow with `N`.
    pub(crate) fn signal(&self) {
        let woken: [Option<Waker>; N] = self.wakers.lock(|cell| {
            // Every write happens inside this critical section, so a plain load + store
            // is enough; `Release` pairs with the `Acquire` in `generation()`.
            let g = self.generation.load(Ordering::Relaxed);
            self.generation.store(g.wrapping_add(1), Ordering::Release);
            let mut slots = cell.borrow_mut();
            core::array::from_fn(|i| slots[i].take())
        });
        for w in woken.into_iter().flatten() {
            w.wake();
        }
    }

    /// The current generation, for a later [`wait_past`](Self::wait_past).
    pub(crate) fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    /// Resolve once the generation differs from `seen`. Race it against a
    /// timer: a full set leaves this future to be polled by that timer's wake.
    pub(crate) async fn wait_past(&self, seen: u32) {
        poll_fn(|cx| {
            if self.generation() != seen {
                return Poll::Ready(());
            }
            let _ = self.register(cx.waker());
            if self.generation() != seen {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
}
