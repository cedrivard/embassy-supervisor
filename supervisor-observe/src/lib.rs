#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Observation facade for [`embassy-supervisor`](https://docs.rs/embassy-supervisor).
//!
//! This crate defines the minimal traits a signal library must implement so the
//! supervisor can verify declared dataflow against live behaviour. The signal
//! library depends only on this tiny crate, and the supervisor depends only on
//! this crate for its observation hooks — the same layering used by `log` and
//! `defmt`.

use portable_atomic::{
    AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicU8, AtomicU16,
    AtomicU32, AtomicU64, AtomicUsize, Ordering,
};

/// A type whose writes can be observed without interpreting the value.
///
/// `change_token()` returns a `u32` whose only meaning is that two unequal
/// readings prove the signal was written between them. It is allowed to wrap,
/// and the supervisor never orders or interprets the numeric value.
///
/// For entries that feed a node's heartbeat (`observed beat`), the token must
/// be counting: each write must advance it, because the heartbeat machinery
/// folds multiple beat entries into a single wrapping sum. Plain value-as-token
/// works for ordinary observation but is unsuitable for beats.
pub trait Observable {
    /// Return a token that changes when the signal is written.
    fn change_token(&self) -> u32;
}

/// A signal that can be written through the supervisor's `put` verb.
pub trait Sink {
    /// The value type written into the signal.
    type Item;

    /// Write `v` into the signal.
    fn put(&self, v: Self::Item);
}

/// A signal that can be read through the supervisor's `get` verb.
pub trait Source {
    /// The value type read from the signal.
    type Item;

    /// Return a snapshot of the signal's current value.
    fn get(&self) -> Self::Item;
}

macro_rules! value_as_token {
    ($($(#[$cfg:meta])* $ty:ty),* $(,)?) => {$(
        $(#[$cfg])*
        impl Observable for $ty {
            fn change_token(&self) -> u32 {
                self.load(Ordering::Relaxed) as u32
            }
        }
    )*};
}
value_as_token!(
    AtomicU8,
    AtomicU16,
    AtomicU32,
    AtomicU64,
    AtomicUsize,
    AtomicI8,
    AtomicI16,
    AtomicI32,
    AtomicI64,
    AtomicIsize,
);
value_as_token!(
    #[cfg(target_has_atomic = "8")]
    core::sync::atomic::AtomicU8,
    #[cfg(target_has_atomic = "8")]
    core::sync::atomic::AtomicI8,
    #[cfg(target_has_atomic = "16")]
    core::sync::atomic::AtomicU16,
    #[cfg(target_has_atomic = "16")]
    core::sync::atomic::AtomicI16,
    #[cfg(target_has_atomic = "32")]
    core::sync::atomic::AtomicU32,
    #[cfg(target_has_atomic = "32")]
    core::sync::atomic::AtomicI32,
    #[cfg(target_has_atomic = "64")]
    core::sync::atomic::AtomicU64,
    #[cfg(target_has_atomic = "64")]
    core::sync::atomic::AtomicI64,
    #[cfg(target_has_atomic = "ptr")]
    core::sync::atomic::AtomicUsize,
    #[cfg(target_has_atomic = "ptr")]
    core::sync::atomic::AtomicIsize,
);

impl Observable for AtomicBool {
    fn change_token(&self) -> u32 {
        self.load(Ordering::Relaxed) as u32
    }
}

#[cfg(target_has_atomic = "8")]
impl Observable for core::sync::atomic::AtomicBool {
    fn change_token(&self) -> u32 {
        self.load(core::sync::atomic::Ordering::Relaxed) as u32
    }
}

macro_rules! value_signal {
    ($($(#[$cfg:meta])* $ty:ty => $item:ty),* $(,)?) => {$(
        $(#[$cfg])*
        impl Sink for $ty {
            type Item = $item;
            #[inline]
            fn put(&self, v: $item) {
                self.store(v, Ordering::Relaxed);
            }
        }
        $(#[$cfg])*
        impl Source for $ty {
            type Item = $item;
            #[inline]
            fn get(&self) -> $item {
                self.load(Ordering::Relaxed)
            }
        }
    )*};
}
value_signal!(
    AtomicU8 => u8,
    AtomicU16 => u16,
    AtomicU32 => u32,
    AtomicU64 => u64,
    AtomicUsize => usize,
    AtomicI8 => i8,
    AtomicI16 => i16,
    AtomicI32 => i32,
    AtomicI64 => i64,
    AtomicIsize => isize,
    AtomicBool => bool,
);
value_signal!(
    #[cfg(target_has_atomic = "8")]
    core::sync::atomic::AtomicU8 => u8,
    #[cfg(target_has_atomic = "8")]
    core::sync::atomic::AtomicI8 => i8,
    #[cfg(target_has_atomic = "8")]
    core::sync::atomic::AtomicBool => bool,
    #[cfg(target_has_atomic = "16")]
    core::sync::atomic::AtomicU16 => u16,
    #[cfg(target_has_atomic = "16")]
    core::sync::atomic::AtomicI16 => i16,
    #[cfg(target_has_atomic = "32")]
    core::sync::atomic::AtomicU32 => u32,
    #[cfg(target_has_atomic = "32")]
    core::sync::atomic::AtomicI32 => i32,
    #[cfg(target_has_atomic = "64")]
    core::sync::atomic::AtomicU64 => u64,
    #[cfg(target_has_atomic = "64")]
    core::sync::atomic::AtomicI64 => i64,
    #[cfg(target_has_atomic = "ptr")]
    core::sync::atomic::AtomicUsize => usize,
    #[cfg(target_has_atomic = "ptr")]
    core::sync::atomic::AtomicIsize => isize,
);

/// A wrapper that counts accesses to an inner signal.
///
/// Use this when the wrapped type does not itself advance a count on every
/// write. `Counted` increments a counter each time [`w`](Self::w) or
/// [`r`](Self::r) is called, so even a rewrite carrying the same value
/// registers as an access. This makes it suitable for heartbeat observation,
/// where the supervisor must see every write.
///
/// Calls through [`inner`](Self::inner) are not counted.
pub struct Counted<T> {
    inner: T,
    writes: AtomicU32,
    reads: AtomicU32,
}

impl<T> Counted<T> {
    /// Wrap `inner` with zero read and write counts.
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            writes: AtomicU32::new(0),
            reads: AtomicU32::new(0),
        }
    }

    /// Borrow the inner signal, counting this as one write.
    pub fn w(&self) -> &T {
        self.writes.fetch_add(1, Ordering::Relaxed);
        &self.inner
    }

    /// Borrow the inner signal, counting this as one read.
    pub fn r(&self) -> &T {
        self.reads.fetch_add(1, Ordering::Relaxed);
        &self.inner
    }

    /// Borrow the inner signal without changing any count.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Return the number of counted writes.
    pub fn writes(&self) -> u32 {
        self.writes.load(Ordering::Relaxed)
    }

    /// Return the number of counted reads.
    pub fn reads(&self) -> u32 {
        self.reads.load(Ordering::Relaxed)
    }
}

impl<T> Observable for Counted<T> {
    fn change_token(&self) -> u32 {
        self.writes()
    }
}

impl<T: Sink> Sink for Counted<T> {
    type Item = T::Item;
    fn put(&self, v: T::Item) {
        self.w().put(v);
    }
}

impl<T: Source> Source for Counted<T> {
    type Item = T::Item;
    fn get(&self) -> T::Item {
        self.r().get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counted_counts_w_and_r_but_not_inner() {
        let c = Counted::new(5u8);
        assert_eq!((c.writes(), c.reads()), (0, 0));
        c.w();
        c.w();
        c.r();
        c.inner();
        assert_eq!((c.writes(), c.reads()), (2, 1));
    }

    #[test]
    fn counted_token_is_the_write_counter() {
        let c = Counted::new(());
        assert_eq!(c.change_token(), 0);
        c.r();
        assert_eq!(c.change_token(), 0);
        c.w();
        assert_eq!(c.change_token(), 1);
    }

    #[test]
    fn counted_hands_back_the_inner_signal() {
        let c = Counted::new(AtomicU32::new(0));
        c.w().store(7, Ordering::Relaxed);
        assert_eq!(c.inner().load(Ordering::Relaxed), 7);
    }

    #[test]
    fn sink_and_source_move_values_through_atomics() {
        let a = AtomicU32::new(0);
        Sink::put(&a, 7);
        assert_eq!(Source::get(&a), 7);
        let b = core::sync::atomic::AtomicI32::new(-1);
        Sink::put(&b, 5);
        assert_eq!(Source::get(&b), 5);
    }

    #[test]
    fn counted_forwards_and_counts_values() {
        let c = Counted::new(AtomicU32::new(0));
        c.put(3);
        assert_eq!(c.get(), 3);
        assert_eq!((c.writes(), c.reads()), (1, 1));
        assert_eq!(c.change_token(), 1, "the count is the token, not the value");
    }

    #[test]
    fn atomics_use_the_value_as_token() {
        let a = AtomicU32::new(0);
        assert_eq!(a.change_token(), 0);
        a.store(3, Ordering::Relaxed);
        assert_eq!(a.change_token(), 3);
        a.store(3, Ordering::Relaxed);
        assert_eq!(a.change_token(), 3);

        let wide = AtomicU64::new(u64::MAX);
        assert_eq!(wide.change_token(), u32::MAX);

        let b = AtomicBool::new(false);
        b.store(true, Ordering::Relaxed);
        assert_eq!(b.change_token(), 1);
    }
}
