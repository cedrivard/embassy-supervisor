//! A signal that remembers when it was last written.

use embassy_time::{Duration, Instant};
use portable_atomic::{AtomicBool, AtomicU32, Ordering};

/// `log2` of the ticks in one stamp unit: the largest power of two not above
/// a millisecond's worth of ticks, one tick below 1 kHz. A shift, so a write
/// never divides.
const SHIFT: u32 = {
    let per_ms = embassy_time::TICK_HZ / 1000;
    if per_ms > 1 { per_ms.ilog2() } else { 0 }
};

/// Where an age saturates instead of wrapping, in stamp units.
const CAP: u32 = u32::MAX / 2;

/// The clock in stamp units, truncated to `u32`.
fn now_units() -> u32 {
    (Instant::now().as_ticks() >> SHIFT) as u32
}

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
/// beside the wrapped value. The stamp is the clock in units of about a
/// millisecond (the largest power-of-two count of ticks that fits in one,
/// or one tick below 1 kHz), truncated to `u32`, so an age is a whole number
/// of units. It saturates at `u32::MAX / 2` units (12 days or more at any
/// tick rate) instead of wrapping, provided something asks at least that
/// often; past that a dead value can read fresh once.
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
        self.stamp.store(now_units(), Ordering::Release);
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

    /// Age in stamp units, saturating at [`CAP`]: past it the stamp is
    /// re-anchored to read exactly `CAP`, so the age never wraps back to
    /// small. The stamp is loaded before the clock is read, so the one used
    /// is never newer than `now`; one stored in between is newer still, and
    /// the compare-exchange leaves it in place.
    fn age_units(&self) -> u32 {
        let last = self.stamp.load(Ordering::Acquire);
        let now = now_units();
        let age = now.wrapping_sub(last);
        if age <= CAP {
            return age;
        }
        let _ = self.stamp.compare_exchange(
            last,
            now.wrapping_sub(CAP),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
        CAP
    }

    /// Time since the last stamped write, `None` until the first: a whole
    /// number of stamp units (see the type doc), saturating at
    /// `u32::MAX / 2` units.
    pub fn age(&self) -> Option<Duration> {
        if !self.written.load(Ordering::Acquire) {
            return None;
        }
        Some(Duration::from_ticks(u64::from(self.age_units()) << SHIFT))
    }

    /// Has the signal been written within `max_age`, to the stamp unit? A
    /// `max_age` at or past the saturation point is clamped below it, so a
    /// dead signal never reads fresh.
    pub fn is_fresh(&self, max_age: Duration) -> bool {
        let bound = max_age.as_ticks().min((u64::from(CAP) << SHIFT) - 1);
        self.age().is_some_and(|age| age.as_ticks() <= bound)
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
