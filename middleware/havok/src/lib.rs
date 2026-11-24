//! Havok Physics middleware tools for Xbox 360 -> PC conversion:
//! - [`packfile`]: Havok packfile reader, pointer fixups, collision extraction, and reflection
//! - [`convert`]: data-driven Xbox 360 -> PC Havok 5.5.0 packfile endian converter and verifier
//! - [`mopp`]: Havok MOPP BVH bytecode decoder and validator
//! - [`port`]: Havok 4.6.1 -> 5.5.0 packfile porting
#![deny(unsafe_code)]

#[allow(dead_code, reason = "shared byte helpers copied from bc2-formats")]
mod bytes;
pub mod convert;
pub mod mopp;
pub mod packfile;
pub mod port;
pub mod pyfmt;
