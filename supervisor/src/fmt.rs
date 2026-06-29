//! Logging macros that forward to [`defmt`] when the `defmt` feature is enabled
//! and expand to (argument-consuming) no-ops otherwise, so the supervisor can log
//! without forcing the `defmt` dependency on consumers. The no-op arms still
//! evaluate-and-discard their arguments (`let _ = &arg`) so enabling/disabling the
//! feature never introduces unused-variable warnings. Pattern adapted from
//! embassy's `fmt.rs` shim.

#![allow(unused_macros)]

#[cfg(feature = "defmt")]
macro_rules! info {
    ($($a:tt)*) => { ::defmt::info!($($a)*) };
}
#[cfg(not(feature = "defmt"))]
macro_rules! info {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(feature = "defmt")]
macro_rules! warn {
    ($($a:tt)*) => { ::defmt::warn!($($a)*) };
}
#[cfg(not(feature = "defmt"))]
macro_rules! warn {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(feature = "defmt")]
macro_rules! error {
    ($($a:tt)*) => { ::defmt::error!($($a)*) };
}
#[cfg(not(feature = "defmt"))]
macro_rules! error {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(feature = "defmt")]
macro_rules! debug {
    ($($a:tt)*) => { ::defmt::debug!($($a)*) };
}
#[cfg(not(feature = "defmt"))]
macro_rules! debug {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(feature = "defmt")]
macro_rules! trace {
    ($($a:tt)*) => { ::defmt::trace!($($a)*) };
}
#[cfg(not(feature = "defmt"))]
macro_rules! trace {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

// Diverging: forwards to defmt's formatter or core's, but always panics.
#[cfg(feature = "defmt")]
macro_rules! panic {
    ($($a:tt)*) => { ::defmt::panic!($($a)*) };
}
#[cfg(not(feature = "defmt"))]
macro_rules! panic {
    ($($a:tt)*) => { ::core::panic!($($a)*) };
}
