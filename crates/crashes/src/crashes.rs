// Crash reporting. The minidump sidecar (crash-handler and minidumper) needs
// per-platform exception handling and a Unix-domain socket, which AROS does
// not have; aros.rs provides the same API there.
#[cfg(not(target_os = "aros"))]
mod real;
#[cfg(not(target_os = "aros"))]
pub use real::*;

#[cfg(target_os = "aros")]
mod aros;
#[cfg(target_os = "aros")]
pub use aros::*;
