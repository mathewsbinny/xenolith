//! Engine-agnostic Xbox 360 Xenos GPU texture untiling (`tiled`, `packed`, `layout`, `untile`, `swap16`,
//! `dxt5a_block`, `rows4`).
//!
//! Engine-specific texture container headers (such as Frostbite's 92-byte `XenonTexture` / `ITexture` wrapper) live
//! in their respective crates under `engines/`.
#![deny(unsafe_code)]

pub mod xenos;

pub use xenos::{
    Place, SurfaceDesc, TextureError, UntiledSurface, XenosFormat, dxt5a_block, layout, layout_raw, packed, rows4,
    swap16, tiled, untile,
};
