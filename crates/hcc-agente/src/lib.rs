//! Agent runtime: long-poll client loop shared by the Linux and Windows agents.

pub mod cliente;
pub mod registro;
#[cfg(feature = "fake")]
pub mod fake;
