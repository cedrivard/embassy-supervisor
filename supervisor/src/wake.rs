//! A broadcast wake for waits that several tasks share: the wave events and
//! the gated-read serving event.

use core::cell::Cell;
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
/// its timer is woken once more, spuriously. `8N + 4` bytes on a 32-bit target.
pub(crate) struct WakerSet<const N: usize> {
    wakers: Mutex<CriticalSectionRawMutex, Cell<[Option<Waker>; N]>>,
    /// Bumped by every `signal`. A waiter reads it before testing its
    /// condition and waits for it to move, so a signal landing between the
    /// test and the park is never missed. Written only under the `wakers` lock.
    generation: AtomicU32,
}

impl<const N: usize> WakerSet<N> {
    pub(crate) const fn new() -> Self {
        Self {
            wakers: Mutex::new(Cell::new([const { None }; N])),
            generation: AtomicU32::new(0),
        }
    }

    /// Register `w` for the next [`signal`](Self::signal). `false` when the
    /// set is full: the caller's own timer has to cover this wait.
    pub(crate) fn register(&self, w: &Waker) -> bool {
        self.wakers.lock(|cell| {
            let slots: &Cell<[Option<Waker>]> = cell;
            let mut free = None;
            for slot in slots.as_slice_of_cells() {
                // `Cell` lends no `&Waker`: take it out, test, put it back.
                let held = slot.take();
                let same = match &held {
                    Some(h) => h.will_wake(w),
                    None => {
                        if free.is_none() {
                            free = Some(slot);
                        }
                        false
                    }
                };
                slot.set(held);
                if same {
                    return true;
                }
            }
            match free {
                Some(slot) => {
                    slot.set(Some(w.clone()));
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
            cell.replace([const { None }; N])
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};
    use std::task::{Wake, Waker};

    use super::WakerSet;

    /// A waker that counts its wakes.
    struct Counting(AtomicUsize);

    impl Wake for Counting {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, StdOrdering::SeqCst);
        }
    }

    fn counting() -> (Arc<Counting>, Waker) {
        let c = Arc::new(Counting(AtomicUsize::new(0)));
        (c.clone(), Waker::from(c))
    }

    fn wakes(c: &Counting) -> usize {
        c.0.load(StdOrdering::SeqCst)
    }

    #[test]
    fn every_registered_waiter_is_woken_once() {
        let set: WakerSet<3> = WakerSet::new();
        let waiters: Vec<_> = (0..3).map(|_| counting()).collect();
        for (_, w) in &waiters {
            assert!(set.register(w));
        }
        set.signal();
        for (c, _) in &waiters {
            assert_eq!(wakes(c), 1);
        }
        // The signal emptied the set: a second one wakes nobody.
        set.signal();
        for (c, _) in &waiters {
            assert_eq!(wakes(c), 1);
        }
    }

    #[test]
    fn the_same_waker_takes_one_slot() {
        let set: WakerSet<2> = WakerSet::new();
        let (a, wa) = counting();
        let (b, wb) = counting();
        assert!(set.register(&wa));
        assert!(set.register(&wa.clone()), "deduplicated, not refused");
        assert!(set.register(&wb), "the duplicate left the second slot free");
        set.signal();
        assert_eq!((wakes(&a), wakes(&b)), (1, 1));
    }

    #[test]
    fn a_full_set_refuses_and_does_not_wake_the_extra_waiter() {
        let set: WakerSet<2> = WakerSet::new();
        let (a, wa) = counting();
        let (b, wb) = counting();
        let (extra, we) = counting();
        assert!(set.register(&wa));
        assert!(set.register(&wb));
        assert!(!set.register(&we));
        set.signal();
        assert_eq!((wakes(&a), wakes(&b), wakes(&extra)), (1, 1, 0));
    }

    #[test]
    fn the_generation_moves_by_one_per_signal() {
        let set: WakerSet<1> = WakerSet::new();
        let g = set.generation();
        set.signal();
        assert_eq!(set.generation(), g.wrapping_add(1));
        set.signal();
        assert_eq!(set.generation(), g.wrapping_add(2));
    }
}
