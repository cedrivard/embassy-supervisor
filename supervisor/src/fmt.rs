#![allow(unused_macros)]

#[cfg(feature = "defmt")]
macro_rules! info {
    ($($a:tt)*) => { ::defmt::info!($($a)*) };
}
#[cfg(all(feature = "log", not(feature = "defmt")))]
macro_rules! info {
    ($($a:tt)*) => { ::log::info!($($a)*) };
}
#[cfg(not(any(feature = "defmt", feature = "log")))]
macro_rules! info {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(feature = "defmt")]
macro_rules! warn {
    ($($a:tt)*) => { ::defmt::warn!($($a)*) };
}
#[cfg(all(feature = "log", not(feature = "defmt")))]
macro_rules! warn {
    ($($a:tt)*) => { ::log::warn!($($a)*) };
}
#[cfg(not(any(feature = "defmt", feature = "log")))]
macro_rules! warn {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(feature = "defmt")]
macro_rules! error {
    ($($a:tt)*) => { ::defmt::error!($($a)*) };
}
#[cfg(all(feature = "log", not(feature = "defmt")))]
macro_rules! error {
    ($($a:tt)*) => { ::log::error!($($a)*) };
}
#[cfg(not(any(feature = "defmt", feature = "log")))]
macro_rules! error {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(feature = "defmt")]
macro_rules! debug {
    ($($a:tt)*) => { ::defmt::debug!($($a)*) };
}
#[cfg(all(feature = "log", not(feature = "defmt")))]
macro_rules! debug {
    ($($a:tt)*) => { ::log::debug!($($a)*) };
}
#[cfg(not(any(feature = "defmt", feature = "log")))]
macro_rules! debug {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(feature = "defmt")]
macro_rules! trace {
    ($($a:tt)*) => { ::defmt::trace!($($a)*) };
}
#[cfg(all(feature = "log", not(feature = "defmt")))]
macro_rules! trace {
    ($($a:tt)*) => { ::log::trace!($($a)*) };
}
#[cfg(not(any(feature = "defmt", feature = "log")))]
macro_rules! trace {
    ($s:literal $(, $a:expr)* $(,)?) => {{ $( let _ = &$a; )* }};
}

#[cfg(feature = "defmt")]
macro_rules! panic {
    ($($a:tt)*) => { ::defmt::panic!($($a)*) };
}
#[cfg(not(feature = "defmt"))]
macro_rules! panic {
    ($($a:tt)*) => { ::core::panic!($($a)*) };
}
