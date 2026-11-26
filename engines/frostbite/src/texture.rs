//! Frostbite 1 / 1.5 `XenonTexture` -> PC `ITexture` adapter (92-byte big-endian / little-endian header wrapper
//! around [`x360_xenos_texture`]).
//!
//! ```
//! use x360_frostbite::texture;
//!
//! // an 8 x 8 L8 texture, one level: the console's 32 x 32 tile (1,024 bytes, a 4 KB storage)
//! let mut b = Vec::new();
//! for v in [3u32, 0, 10, 0, 8, 8, 0, 1] {
//!     b.extend(v.to_be_bytes());
//! }
//! b.extend([0u8; 60]);
//! b.extend(vec![7u8; 4096]);
//! let pc = texture::convert(&b)?;
//! assert_eq!(pc.len(), 92 + 64);
//! assert!(pc[92..].iter().all(|&v| v == 7));
//! # Ok::<(), texture::TextureError>(())
//! ```

use rayon::prelude::*;

pub use x360_xenos_texture::{Place, SurfaceDesc, TextureError, XenosFormat, packed, tiled};

/// The Frostbite texture header: 8 `u32` (version, type, format, flags, width, height, depth, mip count) and 15 level
/// sizes.
pub const HEADER: usize = 92;
/// 64-byte `RES\x02XenonTexture` wrapper prefix found in Frostbite overlay/font archives.
pub const RES_WRAPPER: usize = 64;
/// Texture type 1: a cube map (six faces).
pub const CUBE: u32 = 1;
/// Texture type 2: a 3D volume texture (`depth >= 2` slices).
pub const VOLUME: u32 = 2;
const DXT5A: u32 = 3;
const RGB565: u32 = 5;
const BGRA8: u32 = 9;
const L8: u32 = 10;
/// Header flag bit 1: DICE's build halves the texture for the console.
const HALVED_ON_CONSOLE: u32 = 2;

fn refused(msg: String) -> TextureError {
    TextureError::Refused(msg)
}

/// Maps a Frostbite `XenonTexture` format code to an engine-agnostic [`XenosFormat`].
#[must_use]
pub const fn xenos_format(fmt: u32) -> Option<XenosFormat> {
    Some(match fmt {
        0 | 18 => XenosFormat::Dxt1,
        1 => XenosFormat::Dxt3,
        2 => XenosFormat::Dxt5,
        3 => XenosFormat::Dxt5a,
        5 => XenosFormat::Rgb565,
        9 => XenosFormat::Argb8,
        10 => XenosFormat::L8,
        13 => XenosFormat::Rgba16f,
        19 | 20 => XenosFormat::Bc5,
        _ => return None,
    })
}

/// `(bytes per block, block edge in texels)` of a Frostbite texture format code; `None` for any other.
#[must_use]
pub const fn format(fmt: u32) -> Option<(usize, usize)> {
    match xenos_format(fmt) {
        Some(xf) => Some(xf.block_info()),
        None => None,
    }
}

/// Console layout for a Frostbite texture format code.
#[must_use]
pub fn layout(w: usize, h: usize, mips: usize, fmt: u32, slices: usize) -> Option<(Vec<Vec<Place>>, usize)> {
    let xf = xenos_format(fmt)?;
    Some(x360_xenos_texture::layout(w, h, mips, xf, slices))
}

fn be32(b: &[u8], i: usize) -> u32 {
    let mut v = [0u8; 4];
    if let Some(s) = b.get(4 * i..4 * i + 4) {
        v.copy_from_slice(s);
    }
    u32::from_be_bytes(v)
}

fn header(words: [u32; 8], sizes: &[usize]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER);
    for v in words {
        out.extend(v.to_le_bytes());
    }
    for i in 0..15 {
        out.extend((sizes.get(i).copied().unwrap_or(0) as u32).to_le_bytes());
    }
    out
}

/// Strips a 64-byte `RES\x02` resource wrapper prefix if present, returning the raw 92-byte
/// `XenonTexture` header + tiled payload.
#[must_use]
pub fn unwrap_res_header(b: &[u8]) -> &[u8] {
    if b.get(0..4) == Some(b"RES\x02")
        && let Some(inner) = b.get(RES_WRAPPER..)
        && inner.len() >= HEADER
    {
        return inner;
    }
    b
}

/// Converts a Frostbite console `XenonTexture` resource (92-byte BE header + Xenos tiled payload,
/// with optional 64-byte `RES\x02` wrapper) into a PC `ITexture` resource (92-byte LE header +
/// linear payload). Supports 2D (`typ = 0`), cube (`typ = 1`), and 3D volume (`typ = 2, depth >= 2`)
/// textures.
///
/// # Errors
/// Returns [`TextureError::Refused`] on invalid headers/formats/lengths and [`TextureError::OutOfRange`] on
/// out-of-bounds tiled addresses.
pub fn convert(b: &[u8]) -> Result<Vec<u8>, TextureError> {
    let b = unwrap_res_header(b);
    if b.len() < HEADER {
        return Err(refused("shorter than a texture header".into()));
    }
    let [ver, typ, fmt, flags, w, h, depth, mips] = std::array::from_fn(|i| be32(b, i));
    if !(0 < w && w <= 16384 && 0 < h && h <= 16384 && (1..=15).contains(&mips)) {
        return Err(refused(format!(
            "invalid texture dimensions {w} x {h} / {mips} mip levels"
        )));
    }
    if mips > 32 - w.max(h).leading_zeros() {
        return Err(refused(format!("{mips} mip levels exceed the {w} x {h} chain")));
    }
    let is_volume = typ == VOLUME && (2..=256).contains(&depth);
    if !is_volume && (!(typ == 0 || typ == CUBE) || depth > 1) {
        return Err(refused(format!(
            "texture type {typ} / depth {depth} is not converted (2D and cube maps only)"
        )));
    }
    let Some(xf) = xenos_format(fmt) else {
        return Err(refused(format!(
            "texture format {fmt} is not converted (FORMAT.md 2.4)"
        )));
    };
    if matches!(fmt, DXT5A | RGB565 | BGRA8) && typ != 0 && !is_volume {
        return Err(refused(format!(
            "texture type {typ} in console format {fmt} is not converted (2D only)"
        )));
    }
    let slices = if typ == CUBE {
        6
    } else if is_volume {
        depth as usize
    } else {
        1
    };
    let desc = SurfaceDesc {
        width: w as usize,
        height: h as usize,
        mips: mips as usize,
        slices,
        format: xf,
    };
    let payload = b.get(HEADER..).unwrap_or_default();
    let untiled = x360_xenos_texture::untile(payload, &desc)?;
    let (out_fmt, out_flags, out_depth) = match fmt {
        DXT5A => (L8, flags & !HALVED_ON_CONSOLE, 0),
        RGB565 | BGRA8 => (BGRA8, flags, 0),
        _ => (fmt, flags, depth),
    };
    let words = [ver, typ, out_fmt, out_flags, w, h, out_depth, mips];
    Ok([header(words, &untiled.level_sizes), untiled.data].concat())
}

/// Converts every Frostbite `XenonTexture` in `items` on all cores (rayon's pool), in order.
#[must_use]
pub fn convert_all(items: &[&[u8]]) -> Vec<Result<Vec<u8>, TextureError>> {
    items.par_iter().map(|b| convert(b)).collect()
}

#[cfg(test)]
#[path = "texture_tests.rs"]
mod tests;
