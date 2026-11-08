//! A signal that remembers when it was last written.

use embassy_time::{Duration, Instant};
use portable_atomic::{AtomicBool, AtomicU32, Ordering};

/// A signal wrapper that stamps every write with the time it happened, so a
/// reader can ask how old the value is.
///
/// The read-side half of write freshness. The monitor side already exists:
/// `writes: [X observed beat]` with a `beat_timeout:` reports a node that has
/// not written recently. `Stamped` gives the *consumer* the same fact at the
/// point of use — [`age`](Self::age), [`is_fresh`](Self::is_fresh),
/// [`read_fresh`](Self::read_fresh) — for the case the monitor cannot cover:
/// a value that is still being written but must not be trusted past a
/// certain age. What neither can tell is whether a fresh value is *valid*;
/// a plausible-looking drift needs a consumer that understands the value.
///
/// Writes go through [`w`](Self::w), which stamps them; [`r`](Self::r) and
/// [`inner`](Self::inner) hand out the same `&T` unstamped, so a write
/// through them (an atomic store, a `Signal::signal`) leaves the age alone.
/// There is deliberately no `Deref`: an unstamped access is spelled out. It
/// records the time and nothing else; compose `Stamped<Counted<T>>` when the
/// write count matters too. Costs one `AtomicU32` and one `AtomicBool`
/// beside the wrapped value. The stamp is `Instant` ticks truncated to
/// `u32`, like the node heartbeat: an age saturates at `u32::MAX / 2` ticks
/// (about 35.8 min at a 1 MHz tick) instead of wrapping, provided something
/// asks at least that often.
pub struct Stamped<T> {
    inner: T,
    stamp: AtomicU32,
    written: AtomicBool,
}

impl<T> Stamped<T> {
    /// Wrap `inner`, never written.
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            stamp: AtomicU32::new(0),
            written: AtomicBool::new(false),
        }
    }

    /// Borrow the inner signal for a write, stamping now. The stamp lands
    /// before the write it covers, so until that write completes
    /// [`read_fresh`](Self::read_fresh) vouches for the previous value; `put`
    /// (the `Sink` impl, feature `dataflow`) stamps after its write instead.
    pub fn w(&self) -> &T {
        self.stamp_now();
        &self.inner
    }

    fn stamp_now(&self) {
        self.stamp
            .store(Instant::now().as_ticks() as u32, Ordering::Release);
        self.written.store(true, Ordering::Release);
    }

    /// Borrow the inner signal for a read. Not stamped: a write through it
    /// leaves the age alone.
    pub fn r(&self) -> &T {
        &self.inner
    }

    /// Borrow the inner signal without touching the stamp.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Time since the last stamped write, `None` until the first; saturates at
    /// `u32::MAX / 2` ticks.
    pub fn age(&self) -> Option<Duration> {
        if !self.written.load(Ordering::Acquire) {
            return None;
        }
        Some(Duration::from_ticks(u64::from(crate::saturating_age(
            &self.stamp,
        ))))
    }

    /// Has the signal been written within `max_age`? A `max_age` at or past
    /// the saturation point is clamped below it, so a dead signal never reads
    /// fresh.
    pub fn is_fresh(&self, max_age: Duration) -> bool {
        self.age()
            .is_some_and(|age| age.as_ticks() <= crate::age_bound(max_age))
    }

    /// The inner signal if it was written within `max_age`, else `None`.
    pub fn read_fresh(&self, max_age: Duration) -> Option<&T> {
        self.is_fresh(max_age).then_some(&self.inner)
    }
}

#[cfg(feature = "coupling-observe")]
impl<T: crate::Observable> crate::Observable for Stamped<T> {
    fn change_token(&self) -> u32 {
        self.inner.change_token()
    }
}

#[cfg(feature = "dataflow")]
impl<T: crate::Sink> crate::Sink for Stamped<T> {
    type Item = T::Item;
    fn put(&self, v: T::Item) {
        self.inner.put(v);
        self.stamp_now();
    }
}

#[cfg(feature = "dataflow")]
impl<T: crate::Source> crate::Source for Stamped<T> {
    type Item = T::Item;
    fn get(&self) -> T::Item {
        self.r().get()
    }
}
