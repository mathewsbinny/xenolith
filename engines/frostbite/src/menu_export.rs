//! Pure-Rust exporter for Battlefield 1943 Xbox 360 menu visual (RGBA PNG) and audio (PCM16 WAV)
//! assets directly from the user's Xbox 360 STFS package.
//!
//! Uses [`x360_containers::stfs::Package`], [`crate::fbrb::FbrbArchive`], [`crate::texture::convert`],
//! and [`crate::wavport::decode`] with zero external image/audio crates.

use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::Path;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use x360_containers::stfs::Package;

use crate::fbrb::{FbrbArchive, FbrbError};
use crate::texture;
use crate::wavport;

/// Errors returned while exporting BF1943 Xbox 360 menu assets.
#[derive(Debug, thiserror::Error)]
pub enum MenuExportError {
    /// File I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// STFS package read failure.
    #[error("STFS error: {0}")]
    Stfs(#[from] x360_containers::stfs::Error),
    /// Missing `mainmenu-00.fbrb` inside the STFS container.
    #[error("mainmenu-00.fbrb not found in STFS package")]
    MissingMainMenu,
    /// `FbRB` archive parse error.
    #[error("FbRB error: {0}")]
    Fbrb(#[from] FbrbError),
}

/// Summary of exported BF1943 Xbox 360 menu assets.
#[derive(Debug, Clone, Default)]
pub struct ExportReport {
    /// Number of RGBA PNG textures exported.
    pub pngs_exported: usize,
    /// Number of PCM16 WAV UI sounds exported.
    pub wavs_exported: usize,
    /// Number of BF1943 bitmap fonts exported (`.png` atlas + `.fnt` metrics).
    pub fonts_exported: usize,
    /// List of `(filename, width, height, format)` for exported PNGs.
    pub textures: Vec<(String, u32, u32, u32)>,
}

/// Parsed glyph metrics from a Frostbite `UI.UIFontGlyph` binary DBX entry.
#[derive(Debug, Clone, Copy, Default)]
pub struct FontGlyph {
    /// Unicode codepoint.
    pub code: u32,
    /// Atlas X coordinate in pixels.
    pub x: i32,
    /// Atlas Y coordinate in pixels.
    pub y: i32,
    /// Glyph bitmap width in pixels.
    pub width: i32,
    /// Glyph bitmap height (`Rows`) in pixels.
    pub rows: i32,
    /// Left bearing (`MinX`) in pixels.
    pub min_x: i32,
    /// Vertical baseline offset (`YOffset`) in pixels.
    pub y_offset: i32,
    /// Horizontal cursor advance in pixels.
    pub advance: i32,
}

/// Parses all `UI.UIFontGlyph` records from a Frostbite `{binary}` font `_meta.dbx` file.
#[must_use]
pub fn parse_dbx_font_glyphs(data: &[u8]) -> Vec<FontGlyph> {
    let Ok(file) = crate::dbx::parse(data) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for inst in file.instances() {
        if inst.attr("type") != Some("UI.UIFontGlyph") {
            continue;
        }
        let field_i32 = |name: &str| -> i32 {
            inst.child(name)
                .and_then(crate::dbx::Element::as_i32s)
                .and_then(|v| v.first().copied())
                .unwrap_or(0)
        };
        let code = field_i32("Glyph") as u32;
        if code != 0 {
            out.push(FontGlyph {
                code,
                x: field_i32("X"),
                y: field_i32("Y"),
                width: field_i32("Width"),
                rows: field_i32("Rows"),
                min_x: field_i32("MinX"),
                y_offset: field_i32("YOffset"),
                advance: field_i32("Advance"),
            });
        }
    }
    out
}

/// Parses all `UI.UIFontGlyphKerning` `(first, second, kerning)` records from a Frostbite
/// `{binary}` font `_meta.dbx` file.
#[must_use]
pub fn parse_dbx_font_kerning(data: &[u8]) -> Vec<(u32, u32, i32)> {
    let Ok(file) = crate::dbx::parse(data) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for inst in file.instances() {
        if inst.attr("type") != Some("UI.UIFontGlyphKerning") {
            continue;
        }
        let field_i32 = |name: &str| -> i32 {
            inst.child(name)
                .and_then(crate::dbx::Element::as_i32s)
                .and_then(|v| v.first().copied())
                .unwrap_or(0)
        };
        let first = field_i32("First") as u32;
        let second = field_i32("Second") as u32;
        let kern = field_i32("Kerning");
        if first != 0 && second != 0 && kern != 0 {
            out.push((first, second, kern));
        }
    }
    out
}

fn le32(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

fn rgb565(v: u16) -> [u8; 3] {
    let r = (u32::from((v >> 11) & 31) * 255 / 31) as u8;
    let g = (u32::from((v >> 5) & 63) * 255 / 63) as u8;
    let b = (u32::from(v & 31) * 255 / 31) as u8;
    [r, g, b]
}

fn decode_dxt_color(blk: &[u8], force4: bool) -> ([[u8; 3]; 16], [bool; 16]) {
    let c0 = u16::from_le_bytes([blk.first().copied().unwrap_or(0), blk.get(1).copied().unwrap_or(0)]);
    let c1 = u16::from_le_bytes([blk.get(2).copied().unwrap_or(0), blk.get(3).copied().unwrap_or(0)]);
    let p0 = rgb565(c0);
    let p1 = rgb565(c1);
    let four = c0 > c1 || force4;
    let mix = |a: u8, b: u8, wa: u32, wb: u32, d: u32| -> u8 { ((u32::from(a) * wa + u32::from(b) * wb) / d) as u8 };
    let pal = if four {
        [
            p0,
            p1,
            [
                mix(p0[0], p1[0], 2, 1, 3),
                mix(p0[1], p1[1], 2, 1, 3),
                mix(p0[2], p1[2], 2, 1, 3),
            ],
            [
                mix(p0[0], p1[0], 1, 2, 3),
                mix(p0[1], p1[1], 1, 2, 3),
                mix(p0[2], p1[2], 1, 2, 3),
            ],
        ]
    } else {
        [
            p0,
            p1,
            [
                mix(p0[0], p1[0], 1, 1, 2),
                mix(p0[1], p1[1], 1, 1, 2),
                mix(p0[2], p1[2], 1, 1, 2),
            ],
            [0, 0, 0],
        ]
    };
    let bits = le32(blk, 4);
    let mut colors = [[0u8; 3]; 16];
    let mut opaque = [true; 16];
    for i in 0..16 {
        let code = ((bits >> (2 * i)) & 3) as usize;
        if let Some(dst) = colors.get_mut(i) {
            *dst = pal.get(code).copied().unwrap_or([0; 3]);
        }
        if !four
            && code == 3
            && let Some(op) = opaque.get_mut(i)
        {
            *op = false;
        }
    }
    (colors, opaque)
}

fn decode_dxt5_alpha(blk: &[u8]) -> [u8; 16] {
    let a0 = u32::from(blk.first().copied().unwrap_or(0));
    let a1 = u32::from(blk.get(1).copied().unwrap_or(0));
    let mut pal = [0u8; 8];
    pal[0] = a0 as u8;
    pal[1] = a1 as u8;
    if a0 > a1 {
        for k in 1..=6u32 {
            if let Some(dst) = pal.get_mut((k + 1) as usize) {
                *dst = (((7 - k) * a0 + k * a1) / 7) as u8;
            }
        }
    } else {
        for k in 1..=4u32 {
            if let Some(dst) = pal.get_mut((k + 1) as usize) {
                *dst = (((5 - k) * a0 + k * a1) / 5) as u8;
            }
        }
        pal[6] = 0;
        pal[7] = 255;
    }
    let mut bits: u64 = 0;
    for i in 0..6 {
        bits |= u64::from(blk.get(2 + i).copied().unwrap_or(0)) << (8 * i);
    }
    let mut out = [255u8; 16];
    for i in 0..16 {
        let idx = ((bits >> (3 * i)) & 7) as usize;
        if let Some(dst) = out.get_mut(i) {
            *dst = pal.get(idx).copied().unwrap_or(255);
        }
    }
    out
}

/// Decodes mip 0 of a converted PC `ITexture` into `(width, height, rgba8_pixels)`.
#[must_use]
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
pub fn decode_itexture_rgba(pc_tex: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if pc_tex.len() < texture::HEADER {
        return None;
    }
    let fmt = le32(pc_tex, 8);
    let w = le32(pc_tex, 16) as usize;
    let h = le32(pc_tex, 20) as usize;
    let mip0_size = le32(pc_tex, 32) as usize;
    let data = pc_tex.get(texture::HEADER..texture::HEADER + mip0_size)?;
    if w == 0 || h == 0 {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    match fmt {
        // DXT1
        0 | 18 => {
            let bw = w.div_ceil(4);
            let bh = h.div_ceil(4);
            for by in 0..bh {
                for bx in 0..bw {
                    let off = (by * bw + bx) * 8;
                    let blk = data.get(off..off + 8)?;
                    let (cols, opq) = decode_dxt_color(blk, false);
                    for py in 0..4 {
                        for px in 0..4 {
                            let x = bx * 4 + px;
                            let y = by * 4 + py;
                            if x < w && y < h {
                                let ti = py * 4 + px;
                                let di = (y * w + x) * 4;
                                let c = cols.get(ti).copied().unwrap_or([0; 3]);
                                let a = if opq.get(ti).copied().unwrap_or(true) { 255 } else { 0 };
                                if let Some(dst) = rgba.get_mut(di..di + 4) {
                                    dst.copy_from_slice(&[c[0], c[1], c[2], a]);
                                }
                            }
                        }
                    }
                }
            }
        }
        // DXT3
        1 => {
            let bw = w.div_ceil(4);
            let bh = h.div_ceil(4);
            for by in 0..bh {
                for bx in 0..bw {
                    let off = (by * bw + bx) * 16;
                    let ablk = data.get(off..off + 8)?;
                    let cblk = data.get(off + 8..off + 16)?;
                    let (cols, _) = decode_dxt_color(cblk, true);
                    for py in 0..4 {
                        for px in 0..4 {
                            let x = bx * 4 + px;
                            let y = by * 4 + py;
                            if x < w && y < h {
                                let ti = py * 4 + px;
                                let byte = ablk.get(ti / 2).copied().unwrap_or(0xFF);
                                let nib = if ti % 2 == 0 { byte & 0x0F } else { byte >> 4 };
                                let a = nib * 17;
                                let c = cols.get(ti).copied().unwrap_or([0; 3]);
                                let di = (y * w + x) * 4;
                                if let Some(dst) = rgba.get_mut(di..di + 4) {
                                    dst.copy_from_slice(&[c[0], c[1], c[2], a]);
                                }
                            }
                        }
                    }
                }
            }
        }
        // DXT5
        2 => {
            let bw = w.div_ceil(4);
            let bh = h.div_ceil(4);
            for by in 0..bh {
                for bx in 0..bw {
                    let off = (by * bw + bx) * 16;
                    let ablk = data.get(off..off + 8)?;
                    let cblk = data.get(off + 8..off + 16)?;
                    let alphas = decode_dxt5_alpha(ablk);
                    let (cols, _) = decode_dxt_color(cblk, true);
                    for py in 0..4 {
                        for px in 0..4 {
                            let x = bx * 4 + px;
                            let y = by * 4 + py;
                            if x < w && y < h {
                                let ti = py * 4 + px;
                                let c = cols.get(ti).copied().unwrap_or([0; 3]);
                                let a = alphas.get(ti).copied().unwrap_or(255);
                                let di = (y * w + x) * 4;
                                if let Some(dst) = rgba.get_mut(di..di + 4) {
                                    dst.copy_from_slice(&[c[0], c[1], c[2], a]);
                                }
                            }
                        }
                    }
                }
            }
        }
        // BGRA8 (format 9)
        9 => {
            for i in 0..w * h {
                let s = data.get(i * 4..i * 4 + 4)?;
                if let Some(dst) = rgba.get_mut(i * 4..i * 4 + 4) {
                    dst.copy_from_slice(&[
                        s.get(2).copied().unwrap_or(0),
                        s.get(1).copied().unwrap_or(0),
                        s.first().copied().unwrap_or(0),
                        s.get(3).copied().unwrap_or(255),
                    ]);
                }
            }
        }
        // L8 (format 10)
        10 => {
            for i in 0..w * h {
                let v = data.get(i).copied().unwrap_or(0);
                if let Some(dst) = rgba.get_mut(i * 4..i * 4 + 4) {
                    dst.copy_from_slice(&[v, v, v, 255]);
                }
            }
        }
        _ => return None,
    }
    Some((w as u32, h as u32, rgba))
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if (crc & 1) != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn push_png_chunk(out: &mut Vec<u8>, tag: [u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    let crc_start = out.len();
    out.extend_from_slice(&tag);
    out.extend_from_slice(payload);
    let c = crc32(out.get(crc_start..).unwrap_or_default());
    out.extend_from_slice(&c.to_be_bytes());
}

/// Encodes an RGBA8888 image as a standard PNG file.
///
/// # Errors
/// Returns [`std::io::Error`] if zlib compression fails.
pub fn encode_rgba_png(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut out = Vec::with_capacity(rgba.len() / 2 + 128);
    out.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    push_png_chunk(&mut out, *b"IHDR", &ihdr);

    let stride = (w as usize) * 4;
    let mut raw = Vec::with_capacity((stride + 1) * (h as usize));
    for y in 0..h as usize {
        raw.push(0); // Filter type 0 (None)
        if let Some(row) = rgba.get(y * stride..(y + 1) * stride) {
            raw.extend_from_slice(row);
        }
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(&raw)?;
    let idat = enc.finish()?;
    push_png_chunk(&mut out, *b"IDAT", &idat);
    push_png_chunk(&mut out, *b"IEND", &[]);
    Ok(out)
}

/// Encodes interleaved 16-bit PCM samples as a standard RIFF WAVE file.
#[must_use]
pub fn encode_pcm16_wav(samples: &[i16], channels: u16, sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let block_align = channels * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Tight-crops a sub-rectangle `[rx, ry, rw, rh]` of an RGBA8888 image to its non-transparent bounds
/// (or returns the exact sub-rectangle if `tight == false`) and encodes it as PNG.
fn crop_rgba_png(
    src_w: u32,
    src_h: u32,
    rgba: &[u8],
    rect: [u32; 4],
    tight: bool,
) -> Result<Option<(u32, u32, Vec<u8>)>, std::io::Error> {
    let [rx, ry, rw, rh] = rect;
    let x1 = rx.min(src_w);
    let y1 = ry.min(src_h);
    let x2 = rx.saturating_add(rw).min(src_w);
    let y2 = ry.saturating_add(rh).min(src_h);
    if x1 >= x2 || y1 >= y2 {
        return Ok(None);
    }
    let (mut min_x, mut min_y, mut max_x, mut max_y) = if tight {
        (x2, y2, x1, y1)
    } else {
        (x1, y1, x2 - 1, y2 - 1)
    };
    if tight {
        for y in y1..y2 {
            for x in x1..x2 {
                let a = rgba.get(((y * src_w + x) * 4 + 3) as usize).copied().unwrap_or(0);
                if a > 4 {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
        }
        if min_x > max_x || min_y > max_y {
            return Ok(None);
        }
    }
    let cw = max_x - min_x + 1;
    let ch = max_y - min_y + 1;
    let mut sub = Vec::with_capacity((cw * ch * 4) as usize);
    for y in min_y..=max_y {
        let start = ((y * src_w + min_x) * 4) as usize;
        let end = start + (cw * 4) as usize;
        if let Some(row) = rgba.get(start..end) {
            sub.extend_from_slice(row);
        }
    }
    let png = encode_rgba_png(cw, ch, &sub)?;
    Ok(Some((cw, ch, png)))
}

/// Exports BF1943's literal Xbox 360 menu textures (as RGBA PNGs) and UI SFX (as PCM16 WAVs)
/// from `stfs_path` into `art_out_dir` and `sfx_out_dir`.
///
/// # Errors
/// Returns [`MenuExportError`] if reading the STFS or writing output files fails.
#[allow(clippy::too_many_lines)]
pub fn export_bf1943_menu_assets(
    stfs_path: &Path,
    art_out_dir: &Path,
    sfx_out_dir: &Path,
) -> Result<ExportReport, MenuExportError> {
    fs::create_dir_all(art_out_dir)?;
    fs::create_dir_all(sfx_out_dir)?;

    let file = File::open(stfs_path)?;
    let mut pkg = Package::open(BufReader::new(file))?;
    let mm = pkg
        .entries
        .iter()
        .find(|e| String::from_utf8_lossy(&e.path).eq_ignore_ascii_case("dist/xenon/menu/main/mainmenu-00.fbrb"))
        .cloned()
        .ok_or(MenuExportError::MissingMainMenu)?;

    let raw_fbrb = pkg.read(
        u64::from(mm.start),
        u64::from(mm.count),
        mm.consecutive,
        u64::from(mm.size),
    )?;
    let arc = FbrbArchive::parse(&raw_fbrb)?;
    let mut report = ExportReport::default();

    for entry in &arc.entries {
        let lower = entry.path.to_ascii_lowercase();
        if entry.ext == "XenonTexture" {
            let out_name = if lower == "menu/menu_shared_images_textures_xenon/atlas0.res" {
                Some("menu_shared_images_atlas0.png".to_owned())
            } else if lower == "menu/shared_images_textures_xenon/atlas0.res" {
                Some("shared_images_atlas0.png".to_owned())
            } else if lower == "menu/main_nav_textures_xenon/atlas0.res" {
                Some("main_nav_atlas0.png".to_owned())
            } else if lower == "menu/main_popups_textures_xenon/atlas0.res" {
                Some("main_popups_atlas0.png".to_owned())
            } else if lower == "menu/common_screens_textures_xenon/atlas0.res" {
                Some("common_screens_atlas0.png".to_owned())
            } else if lower == "menu/igm_comp_images_textures_xenon/atlas0.res" {
                Some("igm_comp_images_atlas0.png".to_owned())
            } else if lower == "menu/images/mainnav/controllerxbox.res" {
                Some("controller_xbox.png".to_owned())
            } else if let Some(rest) = lower.strip_prefix("awards/images/ranks/") {
                rest.strip_suffix(".res").map(|stem| format!("rank_{stem}.png"))
            } else {
                None
            };
            if let Some(fname) = out_name
                && let Ok(pc_tex) = texture::convert(&entry.data)
            {
                let fmt = le32(&pc_tex, 8);
                if let Some((w, h, rgba)) = decode_itexture_rgba(&pc_tex) {
                    let png = encode_rgba_png(w, h, &rgba)?;
                    fs::write(art_out_dir.join(&fname), png)?;
                    report.pngs_exported += 1;
                    report.textures.push((fname.clone(), w, h, fmt));

                    // Split the literal 2048x1024 BF1943 main-menu atlas into clean individual sprites:
                    //   - sky_bg.png      (0, 0, 1280, 720)
                    //   - bf1943_logo.png (0, 720, 640, 192)
                    //   - corsair.png     (1280, 0, 768, 540)
                    //   - zero.png        (1280, 540, 768, 484)
                    if fname == "menu_shared_images_atlas0.png" && w == 2048 && h == 1024 {
                        let crops: &[(&str, u32, u32, u32, u32, bool)] = &[
                            ("sky_bg.png", 0, 0, 1280, 720, false),
                            ("bf1943_logo.png", 0, 720, 640, 192, true),
                            ("corsair.png", 1280, 0, 768, 540, true),
                            ("zero.png", 1280, 540, 768, 484, true),
                        ];
                        for &(cname, rx, ry, rw, rh, tight) in crops {
                            if let Some((cw, ch, cpng)) = crop_rgba_png(w, h, &rgba, [rx, ry, rw, rh], tight)? {
                                fs::write(art_out_dir.join(cname), cpng)?;
                                report.pngs_exported += 1;
                                report.textures.push((cname.to_owned(), cw, ch, fmt));
                            }
                        }
                    } else if fname == "main_nav_atlas0.png" && w == 64 && h == 64 {
                        let crops: &[(&str, u32, u32, u32, u32, bool)] = &[
                            ("flag_jp.png", 0, 0, 44, 28, true),
                            ("flag_us.png", 0, 28, 44, 28, true),
                        ];
                        for &(cname, rx, ry, rw, rh, tight) in crops {
                            if let Some((cw, ch, cpng)) = crop_rgba_png(w, h, &rgba, [rx, ry, rw, rh], tight)? {
                                fs::write(art_out_dir.join(cname), cpng)?;
                                report.pngs_exported += 1;
                                report.textures.push((cname.to_owned(), cw, ch, fmt));
                            }
                        }
                    } else if fname == "shared_images_atlas0.png" && w == 1024 && h == 1024 {
                        if let Some((cw, ch, cpng)) = crop_rgba_png(w, h, &rgba, [0, 0, 1024, 720], false)? {
                            fs::write(art_out_dir.join("paper_grain.png"), cpng)?;
                            report.pngs_exported += 1;
                            report.textures.push(("paper_grain.png".to_owned(), cw, ch, fmt));
                        }
                    } else if fname == "main_popups_atlas0.png" && w == 128 && h == 128 {
                        let crops: &[(&str, u32, u32, u32, u32, bool)] = &[
                            ("btn_xbox_b.png", 0, 64, 28, 28, true),
                            ("btn_xbox_y.png", 26, 64, 28, 28, true),
                            ("btn_xbox_a.png", 0, 90, 28, 28, true),
                            ("btn_xbox_x.png", 26, 90, 28, 28, true),
                        ];
                        for &(cname, rx, ry, rw, rh, tight) in crops {
                            if let Some((cw, ch, cpng)) = crop_rgba_png(w, h, &rgba, [rx, ry, rw, rh], tight)? {
                                fs::write(art_out_dir.join(cname), cpng)?;
                                report.pngs_exported += 1;
                                report.textures.push((cname.to_owned(), cw, ch, fmt));
                            }
                        }
                    } else if fname == "igm_comp_images_atlas0.png" && w == 512 && h == 1024 {
                        let crops: &[(&str, u32, u32, u32, u32, bool)] = &[
                            ("controller_360_hd.png", 0, 0, 320, 212, true),
                            ("headline_bar.png", 0, 384, 512, 74, true),
                            ("subheadline_bar.png", 104, 668, 260, 42, true),
                            ("soldier_us_rifleman.png", 0, 464, 134, 200, true),
                            ("soldier_us_infantry.png", 142, 464, 130, 200, true),
                            ("soldier_us_scout.png", 274, 464, 118, 200, true),
                            ("soldier_jp_rifleman.png", 392, 480, 118, 216, true),
                            ("soldier_jp_infantry.png", 0, 666, 108, 204, true),
                            ("soldier_jp_scout.png", 322, 0, 134, 206, true),
                        ];
                        for &(cname, rx, ry, rw, rh, tight) in crops {
                            if let Some((cw, ch, cpng)) = crop_rgba_png(w, h, &rgba, [rx, ry, rw, rh], tight)? {
                                fs::write(art_out_dir.join(cname), cpng)?;
                                report.pngs_exported += 1;
                                report.textures.push((cname.to_owned(), cw, ch, fmt));
                            }
                        }
                    }
                }
            }
        } else if entry.ext == "Wave" {
            let wav_name = match lower.as_str() {
                "sound/beach_sfx/ui/bsfx_ui_fetick_01_wave.res" => Some("ui_tick.wav"),
                "sound/beach_sfx/ui/bsfx_ui_fescroll_01_wave.res" => Some("ui_scroll.wav"),
                "sound/beach_sfx/ui/bsfx_ui_feforward_01_wave.res" => Some("ui_forward.wav"),
                "sound/beach_sfx/ui/bsfx_ui_febackwards_01_wave.res" => Some("ui_backwards.wav"),
                _ => None,
            };
            if let Some(fname) = wav_name
                && let Some((pcm, ch, rate)) = wavport::decode(&entry.data, "bf1943", &[None, None, None])
            {
                let wav = encode_pcm16_wav(&pcm, ch as u16, rate);
                fs::write(sfx_out_dir.join(fname), wav)?;
                report.wavs_exported += 1;
            }
        }
    }
    // Export BF1943's literal Xbox 360 US bitmap fonts (`EurostileLTStd-BoldCn`) from `build_overlay-00.fbrb`
    if let Some(ov) = pkg
        .entries
        .iter()
        .find(|e| String::from_utf8_lossy(&e.path).eq_ignore_ascii_case("dist/xenon/build_overlay-00.fbrb"))
        .cloned()
        && let Ok(ov_raw) = pkg.read(
            u64::from(ov.start),
            u64::from(ov.count),
            ov.consecutive,
            u64::from(ov.size),
        )
        && let Ok(ov_arc) = FbrbArchive::parse(&ov_raw)
    {
        let fonts_dir = art_out_dir
            .parent()
            .and_then(Path::parent)
            .map_or_else(|| art_out_dir.join("fonts"), |m| m.join("fonts"));
        fs::create_dir_all(&fonts_dir)?;

        for font_name in [
            "headline_large",
            "headline_medium",
            "headline_small",
            "normal_large",
            "normal_medium",
            "normal_small",
            "list_header",
            "list_item",
        ] {
            let tex_path = format!("output/xenon/fonts/us/{font_name}_texture.res");
            let meta_path = format!("output/xenon/fonts/us/{font_name}_meta.dbx");
            if let (Some(tex_e), Some(meta_e)) = (
                ov_arc.entries.iter().find(|e| e.path.eq_ignore_ascii_case(&tex_path)),
                ov_arc.entries.iter().find(|e| e.path.eq_ignore_ascii_case(&meta_path)),
            ) && let Ok(pc_tex) = texture::convert(&tex_e.data)
                && let Some((w, h, rgba)) = decode_itexture_rgba(&pc_tex)
            {
                let glyphs = parse_dbx_font_glyphs(&meta_e.data);
                let kerns = parse_dbx_font_kerning(&meta_e.data);
                if !glyphs.is_empty() {
                    let png = encode_rgba_png(w, h, &rgba)?;
                    fs::write(fonts_dir.join(format!("{font_name}.png")), png)?;
                    let mut fnt = String::with_capacity(glyphs.len() * 36 + kerns.len() * 16);
                    for g in &glyphs {
                        use std::fmt::Write as _;
                        let _ = writeln!(
                            fnt,
                            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            g.code, g.x, g.y, g.width, g.rows, g.min_x, g.y_offset, g.advance
                        );
                    }
                    for &(first, second, kern) in &kerns {
                        use std::fmt::Write as _;
                        let _ = writeln!(fnt, "K\t{first}\t{second}\t{kern}");
                    }
                    fs::write(fonts_dir.join(format!("{font_name}.fnt")), fnt)?;
                    report.fonts_exported += 1;
                }
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbx::{DbxFile, Element, KIND_CONTAINER};

    #[test]
    fn encodes_png_and_wav_headers() {
        let png = encode_rgba_png(2, 2, &[255u8; 16]).ok();
        assert!(
            png.as_ref()
                .is_some_and(|b| b.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]))
        );
        let wav = encode_pcm16_wav(&[0, 1000, -1000, 0], 2, 44_100);
        assert!(wav.starts_with(b"RIFF"));
        assert_eq!(wav.get(8..12), Some(b"WAVE".as_slice()));
        assert_eq!(wav.len(), 44 + 8);
    }

    #[test]
    fn parses_glyph_and_kerning_from_synthetic_dbx() {
        let glyph_inst = Element {
            tag: "instance".to_owned(),
            kind: KIND_CONTAINER,
            attributes: vec![
                ("guid".to_owned(), "G1".to_owned()),
                ("type".to_owned(), "UI.UIFontGlyph".to_owned()),
            ],
            text: String::new(),
            raw: Vec::new(),
            children: vec![
                Element::new_i32_field("Glyph", &[65]),
                Element::new_i32_field("X", &[10]),
                Element::new_i32_field("Y", &[20]),
                Element::new_i32_field("Width", &[14]),
                Element::new_i32_field("Rows", &[18]),
                Element::new_i32_field("MinX", &[1]),
                Element::new_i32_field("YOffset", &[5]),
                Element::new_i32_field("Advance", &[16]),
            ],
        };
        let kern_inst = Element {
            tag: "instance".to_owned(),
            kind: KIND_CONTAINER,
            attributes: vec![
                ("guid".to_owned(), "K1".to_owned()),
                ("type".to_owned(), "UI.UIFontGlyphKerning".to_owned()),
            ],
            text: String::new(),
            raw: Vec::new(),
            children: vec![
                Element::new_i32_field("First", &[65]),
                Element::new_i32_field("Second", &[86]),
                Element::new_i32_field("Kerning", &[-2]),
            ],
        };
        let part = Element {
            tag: "partition".to_owned(),
            kind: KIND_CONTAINER,
            attributes: vec![("guid".to_owned(), "P1".to_owned())],
            text: String::new(),
            raw: Vec::new(),
            children: vec![glyph_inst, kern_inst],
        };
        let bytes = DbxFile { roots: vec![part] }.to_bytes();

        let glyphs = parse_dbx_font_glyphs(&bytes);
        assert_eq!(glyphs.len(), 1);
        assert_eq!(
            glyphs.first().map(|g| (g.code, g.x, g.y, g.advance)),
            Some((65, 10, 20, 16))
        );

        let kerns = parse_dbx_font_kerning(&bytes);
        assert_eq!(kerns, vec![(65, 86, -2)]);
    }
}
