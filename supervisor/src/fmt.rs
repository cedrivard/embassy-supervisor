#![allow(unused_macros)]

// Backend selection is per TARGET as well as per feature: `defmt` is an
// embedded protocol — a binary carrying defmt statements needs a
// `#[global_logger]` sink to link, which only firmware has — so the defmt
// arms are gated to `target_os = "none"`. On a hosted target the same
// feature set falls through to `log` (if enabled) or to the no-op arm, and
// nothing references the `_defmt_*` linker symbols. One feature list
// therefore serves both halves of a SITL setup: defmt on hardware, the
// `log` facade on the host (see `init_host_logging`).

#[cfg(all(feature = "defmt", target_os = "none"))]
macro_rules! info {
    ($($a:tt)*) => { ::defmt::info!($($a)*) };
}
#[cfg(all(feature = "log", not(all(feature = "defmt", target_os = "none"))))]
macro_rules! info {
    ($($a:tt)*) => { ::log::info!($($a)*) };
}
#[cfg(not(any(all(feature = "defmt", target_os = "none"), feature = "log")))]
macro_rules! info {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(all(feature = "defmt", target_os = "none"))]
macro_rules! warn {
    ($($a:tt)*) => { ::defmt::warn!($($a)*) };
}
#[cfg(all(feature = "log", not(all(feature = "defmt", target_os = "none"))))]
macro_rules! warn {
    ($($a:tt)*) => { ::log::warn!($($a)*) };
}
#[cfg(not(any(all(feature = "defmt", target_os = "none"), feature = "log")))]
macro_rules! warn {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(all(feature = "defmt", target_os = "none"))]
macro_rules! error {
    ($($a:tt)*) => { ::defmt::error!($($a)*) };
}
#[cfg(all(feature = "log", not(all(feature = "defmt", target_os = "none"))))]
macro_rules! error {
    ($($a:tt)*) => { ::log::error!($($a)*) };
}
#[cfg(not(any(all(feature = "defmt", target_os = "none"), feature = "log")))]
macro_rules! error {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(all(feature = "defmt", target_os = "none"))]
macro_rules! debug {
    ($($a:tt)*) => { ::defmt::debug!($($a)*) };
}
#[cfg(all(feature = "log", not(all(feature = "defmt", target_os = "none"))))]
macro_rules! debug {
    ($($a:tt)*) => { ::log::debug!($($a)*) };
}
#[cfg(not(any(all(feature = "defmt", target_os = "none"), feature = "log")))]
macro_rules! debug {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(all(feature = "defmt", target_os = "none"))]
macro_rules! trace {
    ($($a:tt)*) => { ::defmt::trace!($($a)*) };
}
#[cfg(all(feature = "log", not(all(feature = "defmt", target_os = "none"))))]
macro_rules! trace {
    ($($a:tt)*) => { ::log::trace!($($a)*) };
}
#[cfg(not(any(all(feature = "defmt", target_os = "none"), feature = "log")))]
macro_rules! trace {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(all(feature = "defmt", target_os = "none"))]
macro_rules! panic {
    ($($a:tt)*) => { ::defmt::panic!($($a)*) };
}
#[cfg(not(all(feature = "defmt", target_os = "none")))]
macro_rules! panic {
    ($($a:tt)*) => { ::core::panic!($($a)*) };
}
