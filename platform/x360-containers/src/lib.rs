//! Read-only readers for Xbox 360 container formats: STFS packages (`LIVE` / `PIRS` / `CON`),
//! XISO disc images, and `XEX2` executables.
#![deny(unsafe_code)]

pub mod stfs;
pub mod xex;
pub mod xiso;
