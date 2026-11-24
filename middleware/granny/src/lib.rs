//! RAD Game Tools Granny 2 (`.gr2`) middleware tools for Xbox 360 -> PC conversion:
//! - [`format`]: Granny 2 reader (PC and Xbox 360), tables, object trees, and bone-table renumbering
//! - [`convert`]: Xbox 360 -> PC big-endian to little-endian conversion (`to_pc`, `to_pc_all`)
//! - [`edit`]: Granny CRC recomputation, track addition, position shifting, and orientation curve writing
#![deny(unsafe_code)]

pub mod convert;
pub mod edit;
pub mod format;

/// Standard IEEE CRC-32 (used by Granny file headers).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut h = flate2::Crc::new();
    h.update(data);
    h.sum()
}
