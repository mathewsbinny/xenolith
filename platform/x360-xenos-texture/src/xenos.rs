//! Engine-agnostic Xbox 360 (Xenos GPU) 2D / cube texture untiling, mip-tail packing, 16-bit endian swap, and
//! console-only format decoding (DXT5A -> L8, RGB565 -> BGRA8, big-endian A8R8G8B8 -> BGRA8).
//!
//! This module operates strictly on raw pixel/block payloads and explicit [`SurfaceDesc`] metadata — it knows nothing
//! about any game engine's texture container header (DDS, Frostbite `XenonTexture`, Unreal `Texture2D`, etc.).
//!
//! ```
//! use x360_xenos_texture::{SurfaceDesc, XenosFormat, untile};
//!
//! // An 8 x 8 L8 surface, 1 mip: one 32 x 32-block Xenos tile padded to 4 KB.
//! let desc = SurfaceDesc {
//!     width: 8,
//!     height: 8,
//!     mips: 1,
//!     slices: 1,
//!     format: XenosFormat::L8,
//! };
//! let raw = vec![7u8; 4096];
//! let pc = untile(&raw, &desc)?;
//! assert_eq!(pc.level_sizes, vec![64]);
//! assert_eq!(pc.data.len(), 64);
//! assert!(pc.data.iter().all(|&v| v == 7));
//! # Ok::<(), x360_xenos_texture::TextureError>(())
//! ```

use thiserror::Error;

/// Pixel / block format of an Xbox 360 Xenos texture surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum XenosFormat {
    /// BC1 / DXT1 (8 bytes per 4x4 block).
    Dxt1,
    /// BC2 / DXT3 (16 bytes per 4x4 block).
    Dxt3,
    /// BC3 / DXT5 (16 bytes per 4x4 block).
    Dxt5,
    /// Single-channel DXT5 alpha block (8 bytes per 4x4 block), decoded to 8-bit L8 (4-byte row-aligned).
    Dxt5a,
    /// BC5 / 3Dc / ATI2 normal map (16 bytes per 4x4 block).
    Bc5,
    /// 16-bit RGB565 (2 bytes per texel), decoded to little-endian B8G8R8A8 (alpha 255).
    Rgb565,
    /// Big-endian A8R8G8B8 (4 bytes per texel), swizzled to little-endian B8G8R8A8.
    Argb8,
    /// 8-bit luminance / single-channel (1 byte per texel, 4-byte row-aligned on PC).
    L8,
    /// 64-bit RGBA16F (8 bytes per texel).
    Rgba16f,
}

impl XenosFormat {
    /// `(bytes_per_block, block_edge_in_texels)` on the Xenos GPU.
    #[must_use]
    pub const fn block_info(self) -> (usize, usize) {
        match self {
            Self::Dxt1 | Self::Dxt5a => (8, 4),
            Self::Dxt3 | Self::Dxt5 | Self::Bc5 => (16, 4),
            Self::Rgba16f => (8, 1),
            Self::Rgb565 => (2, 1),
            Self::L8 => (1, 1),
            Self::Argb8 => (4, 1),
        }
    }
}

/// Description of a raw tiled Xbox 360 texture payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceDesc {
    /// Base level width in texels (`1..=16384`).
    pub width: usize,
    /// Base level height in texels (`1..=16384`).
    pub height: usize,
    /// Number of mip levels (`1..=15`).
    pub mips: usize,
    /// Number of array slices / faces (`1` for 2D, `6` for cube maps).
    pub slices: usize,
    /// Console pixel/block format.
    pub format: XenosFormat,
}

/// Linear (untiled) PC texture payload produced by [`untile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntiledSurface {
    /// Byte length of each mip level for a single slice (slice 0).
    pub level_sizes: Vec<usize>,
    /// Linear pixel/block bytes, ordered slice-by-slice, mip-by-mip.
    pub data: Vec<u8>,
}

/// Why a texture surface could not be untiled.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TextureError {
    /// Invalid input dimensions, format, or truncated payload.
    #[error("{0}")]
    Refused(String),
    /// The computed tiled offset reads past the end of the input payload.
    #[error(
        "the layout reads past the end of the texture data (a level outside the console storage: a shape no texture of either game has)"
    )]
    OutOfRange,
}

fn refused(msg: String) -> TextureError {
    TextureError::Refused(msg)
}

fn log2c(v: usize) -> usize {
    let mut n = 0;
    while (1usize << n) < v {
        n += 1;
    }
    n
}

fn align(v: usize, a: usize) -> usize {
    v.div_ceil(a) * a
}

/// Xenos 2D tiling (`XGAddress2DTiledOffset`): block `(x, y)` of a surface `pitch` blocks wide -> block index.
#[must_use]
pub fn tiled(x: usize, y: usize, pitch: usize, log_bpp: u32) -> usize {
    let aw = (pitch + 31) & !31;
    let macro_ = ((x >> 5) + (y >> 5) * (aw >> 5)) << (log_bpp + 7);
    let micro = ((x & 7) + ((y & 6) << 2)) << log_bpp;
    let off = macro_ + ((micro & !15) << 1) + (micro & 15) + ((y & 8) << (3 + log_bpp)) + ((y & 1) << 4);
    (((off & !511) << 3)
        + ((off & 0x1C0) << 2)
        + (off & 63)
        + ((y & 16) << 7)
        + (((((y & 8) >> 2) + (x >> 3)) & 3) << 6))
        >> log_bpp
}

/// Block offset `(x, y)` of mip level `mip` inside the packed tail tile, or `None` when the level has its own
/// storage. A level is packed once its shorter side is 16 texels or less.
#[must_use]
pub fn packed(w: usize, h: usize, mip: usize, edge: usize) -> Option<(usize, usize)> {
    let (lw, lh) = (log2c(w), log2c(h));
    let ls = lw.min(lh);
    if ls > 4 + mip {
        return None;
    }
    let base = ls.saturating_sub(4);
    let pm = mip - base;
    let (x, y) = if pm < 3 {
        if lw > lh { (0, 16 >> pm) } else { (16 >> pm, 0) }
    } else if lw > lh {
        ((1usize << (lw - base)) >> (pm - 2), 0)
    } else {
        (0, (1usize << (lh - base)) >> (pm - 2))
    };
    Some((x / edge, y / edge))
}

/// Where a level lies in the console data: its storage's byte start, pitch in blocks, and block offset `(x, y)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    /// Byte start of the level's storage.
    pub start: usize,
    /// Pitch in blocks.
    pub pitch: usize,
    /// Block offset on x (the packed tail tile).
    pub x: usize,
    /// Block offset on y.
    pub y: usize,
}

/// Per slice, per level: where each level lies in the console data, plus the total console payload size in bytes.
/// Level 0 of every slice comes first, then level by level (each level all slices); every storage is 4 KB aligned.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn layout_raw(w: usize, h: usize, mips: usize, bpb: usize, edge: usize, slices: usize) -> (Vec<Vec<Place>>, usize) {
    let blocks = |v: usize| v.div_ceil(edge).max(1);
    let (bw, bh) = (blocks(w), blocks(h));
    let base_pitch = align(bw, 32);
    let base_size = align(base_pitch * align(bh, 32) * bpb, 4096);
    let mut out: Vec<Vec<Place>> = vec![Vec::with_capacity(mips); slices];
    let mut pos = base_size * slices;
    let mut tail: Option<(usize, usize, usize)> = None;
    for m in 0..mips {
        let pk = packed(w, h, m, edge);
        if m == 0 {
            if pk.is_some() {
                tail = Some((0, base_pitch, base_size));
            }
            let (x, y) = pk.unwrap_or((0, 0));
            for (s, levels) in out.iter_mut().enumerate() {
                levels.push(Place {
                    start: s * base_size,
                    pitch: base_pitch,
                    x,
                    y,
                });
            }
            continue;
        }
        let sbw = blocks(((1usize << log2c(w)) >> m).max(1));
        let sbh = blocks(((1usize << log2c(h)) >> m).max(1));
        let pitch = align(sbw, 32);
        let size = align(pitch * align(sbh, 32) * bpb, 4096);
        match pk {
            None => {
                for (s, levels) in out.iter_mut().enumerate() {
                    levels.push(Place {
                        start: pos + s * size,
                        pitch,
                        x: 0,
                        y: 0,
                    });
                }
                pos += size * slices;
            }
            Some((x, y)) => {
                let t = *tail.get_or_insert_with(|| {
                    let t = (pos, pitch, size);
                    pos += size * slices;
                    t
                });
                for (s, levels) in out.iter_mut().enumerate() {
                    levels.push(Place {
                        start: t.0 + s * t.2,
                        pitch: t.1,
                        x,
                        y,
                    });
                }
            }
        }
    }
    (out, pos)
}

/// Convenience wrapper over [`layout_raw`] taking a [`XenosFormat`].
#[must_use]
pub fn layout(w: usize, h: usize, mips: usize, fmt: XenosFormat, slices: usize) -> (Vec<Vec<Place>>, usize) {
    let (bpb, edge) = fmt.block_info();
    layout_raw(w, h, mips, bpb, edge, slices)
}

/// 16-bit words byte-swapped in place, as the Xbox 360 GPU stores tiled block/half-word data.
pub fn swap16(data: &mut [u8]) {
    for pair in data.as_chunks_mut::<2>().0 {
        pair.swap(0, 1);
    }
}

/// Pads an 8-bit level (`h` rows of `w` bytes) so each row is a multiple of 4 bytes.
#[must_use]
pub fn rows4(level: &[u8], w: usize, h: usize) -> Vec<u8> {
    let pitch = w.div_ceil(4) * 4;
    if pitch == w {
        return level.to_vec();
    }
    let mut out = vec![0u8; pitch * h];
    for (row, src) in out.chunks_exact_mut(pitch).zip(level.chunks(w)) {
        for (d, &v) in row.iter_mut().zip(src) {
            *d = v;
        }
    }
    out
}

/// Decodes one 8-byte DXT5A / BC4 single-channel block -> 4x4 bytes, rows first.
#[must_use]
pub fn dxt5a_block(block: [u8; 8]) -> [u8; 16] {
    let [a0, a1, rest @ ..] = block;
    let (a0, a1) = (u32::from(a0), u32::from(a1));
    let bits = rest
        .iter()
        .enumerate()
        .fold(0u64, |bits, (k, &v)| bits | (u64::from(v) << (8 * k)));
    let pal: [u32; 8] = std::array::from_fn(|i| {
        let i = i as u32;
        match i {
            0 => a0,
            1 => a1,
            _ if a0 > a1 => ((8 - i) * a0 + (i - 1) * a1) / 7,
            2..=5 => ((6 - i) * a0 + (i - 1) * a1) / 5,
            6 => 0,
            _ => 255,
        }
    });
    std::array::from_fn(|i| pal.get(((bits >> (3 * i)) & 7) as usize).copied().unwrap_or(0) as u8)
}

fn slice(data: &[u8], at: usize, n: usize) -> Result<&[u8], TextureError> {
    data.get(at..at + n).ok_or(TextureError::OutOfRange)
}

fn array<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N], TextureError> {
    data.get(at..at + N)
        .and_then(|s| s.try_into().ok())
        .ok_or(TextureError::OutOfRange)
}

fn level_size(w: usize, h: usize, m: usize) -> (usize, usize) {
    ((w >> m).max(1), (h >> m).max(1))
}

/// Untiles a raw Xbox 360 Xenos texture payload described by `desc` into linear PC mip levels.
///
/// # Errors
/// Returns [`TextureError::Refused`] if the payload is shorter than the tiled layout requires or if a 2D-only
/// decoded format is given multiple slices, and [`TextureError::OutOfRange`] if a tiled block address exceeds the
/// payload bounds.
#[allow(clippy::too_many_lines)]
pub fn untile(raw: &[u8], desc: &SurfaceDesc) -> Result<UntiledSurface, TextureError> {
    let (w, h, mips, slices) = (desc.width, desc.height, desc.mips, desc.slices);
    let (where_, size) = layout(w, h, mips, desc.format, slices);
    if raw.len() < size {
        return Err(refused(format!(
            "{} bytes of texture data, the layout needs {size}",
            raw.len()
        )));
    }

    match desc.format {
        XenosFormat::Dxt5a => {
            if slices != 1 {
                return Err(refused("DXT5A untiling supports 2D surfaces (1 slice) only".into()));
            }
            let mut sizes = Vec::with_capacity(mips);
            let mut body = Vec::new();
            for (m, p) in where_.first().into_iter().flatten().enumerate() {
                let (lw, lh) = level_size(w, h, m);
                let (bw, bh) = (lw.div_ceil(4).max(1), lh.div_ceil(4).max(1));
                let iw = bw * 4;
                let mut img = vec![0u8; iw * bh * 4];
                for y in 0..bh {
                    for x in 0..bw {
                        let s = p.start + tiled(x + p.x, y + p.y, p.pitch, 3) * 8;
                        let mut blk = array::<8>(raw, s)?;
                        swap16(&mut blk);
                        let px = dxt5a_block(blk);
                        for (r, row) in px.as_chunks::<4>().0.iter().enumerate() {
                            let at = (y * 4 + r) * iw + x * 4;
                            if let Some(dst) = img.get_mut(at..at + 4) {
                                dst.copy_from_slice(row);
                            }
                        }
                    }
                }
                let mut level = Vec::with_capacity(lw * lh);
                for row in img.chunks_exact(iw).take(lh) {
                    level.extend(row.iter().take(lw));
                }
                let level = rows4(&level, lw, lh);
                sizes.push(level.len());
                body.extend(level);
            }
            Ok(UntiledSurface {
                level_sizes: sizes,
                data: body,
            })
        }
        XenosFormat::Rgb565 => {
            if slices != 1 {
                return Err(refused("RGB565 untiling supports 2D surfaces (1 slice) only".into()));
            }
            let mut sizes = Vec::with_capacity(mips);
            let mut body = Vec::new();
            for (m, p) in where_.first().into_iter().flatten().enumerate() {
                let (lw, lh) = level_size(w, h, m);
                let mut level = Vec::with_capacity(lw * lh * 4);
                for y in 0..lh {
                    for x in 0..lw {
                        let s = p.start + tiled(x + p.x, y + p.y, p.pitch, 1) * 2;
                        let [hi, lo] = array(raw, s)?;
                        let v = u32::from(lo) | (u32::from(hi) << 8);
                        let (r5, g6, b5) = ((v >> 11) & 31, (v >> 5) & 63, v & 31);
                        level.extend([
                            ((b5 << 3) | (b5 >> 2)) as u8,
                            ((g6 << 2) | (g6 >> 4)) as u8,
                            ((r5 << 3) | (r5 >> 2)) as u8,
                            255,
                        ]);
                    }
                }
                sizes.push(level.len());
                body.extend(level);
            }
            Ok(UntiledSurface {
                level_sizes: sizes,
                data: body,
            })
        }
        XenosFormat::Argb8 => {
            if slices != 1 {
                return Err(refused("ARGB8 untiling supports 2D surfaces (1 slice) only".into()));
            }
            let data = raw.get(..size).unwrap_or(raw);
            let mut sizes = Vec::with_capacity(mips);
            let mut body = Vec::new();
            for (m, p) in where_.first().into_iter().flatten().enumerate() {
                let (lw, lh) = level_size(w, h, m);
                let mut level = Vec::with_capacity(lw * lh * 4);
                for y in 0..lh {
                    for x in 0..lw {
                        let s = p.start + tiled(x + p.x, y + p.y, p.pitch, 2) * 4;
                        let [alpha, red, green, blue] = array(data, s)?;
                        level.extend([blue, green, red, alpha]);
                    }
                }
                sizes.push(level.len());
                body.extend(level);
            }
            Ok(UntiledSurface {
                level_sizes: sizes,
                data: body,
            })
        }
        _ => {
            let (bpb, edge) = desc.format.block_info();
            let log_bpp = match bpb {
                1 => 0,
                8 => 3,
                16 => 4,
                _ => return Err(TextureError::OutOfRange),
            };
            let mut sizes = Vec::with_capacity(mips);
            let mut body = Vec::new();
            for (s, levels) in where_.iter().enumerate() {
                for (m, p) in levels.iter().enumerate() {
                    let (lw, lh) = level_size(w, h, m);
                    let (lbw, lbh) = (lw.div_ceil(edge).max(1), lh.div_ceil(edge).max(1));
                    let mut lev = Vec::with_capacity(lbw * lbh * bpb);
                    for y in 0..lbh {
                        for x in 0..lbw {
                            let src = p.start + tiled(x + p.x, y + p.y, p.pitch, log_bpp) * bpb;
                            lev.extend_from_slice(slice(raw, src, bpb)?);
                        }
                    }
                    if bpb > 1 {
                        swap16(&mut lev);
                    }
                    let lev = if bpb == 1 { rows4(&lev, lbw, lbh) } else { lev };
                    if s == 0 {
                        sizes.push(lev.len());
                    }
                    body.extend(lev);
                }
            }
            Ok(UntiledSurface {
                level_sizes: sizes,
                data: body,
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn untiles_dxt1_and_l8_without_engine_headers() {
        let desc = SurfaceDesc {
            width: 8,
            height: 8,
            mips: 1,
            slices: 1,
            format: XenosFormat::Dxt1,
        };
        let (_, need) = layout(8, 8, 1, XenosFormat::Dxt1, 1);
        let raw = vec![0xAA; need];
        let out = untile(&raw, &desc).unwrap();
        assert_eq!(out.level_sizes, vec![32]);
        assert_eq!(out.data.len(), 32);
    }

    #[test]
    fn helpers() {
        assert_eq!(rows4(&[1, 2, 3, 4, 5, 6], 3, 2), [1, 2, 3, 0, 4, 5, 6, 0]);
        assert_eq!(rows4(&[1, 2, 3, 4], 4, 1), [1, 2, 3, 4]);
        let mut d = [1, 2, 3];
        swap16(&mut d);
        assert_eq!(d, [2, 1, 3]);
        assert_eq!(slice(&[1, 2], 1, 2), Err(TextureError::OutOfRange));
        assert_eq!(array::<2>(&[1, 2], 1), Err(TextureError::OutOfRange));
        // 6-value mode (a0 <= a1): indices 6 and 7 are 0 and 255
        let px = dxt5a_block([10, 20, 0b1111_1110, 0xFF, 0, 0, 0, 0]);
        assert_eq!(&px[..4], &[0, 255, 255, 255]);
    }

    #[test]
    fn rejects_short_payload() {
        let desc = SurfaceDesc {
            width: 32,
            height: 32,
            mips: 1,
            slices: 1,
            format: XenosFormat::Dxt5,
        };
        assert!(matches!(untile(&[0u8; 64], &desc), Err(TextureError::Refused(_))));
    }
}
